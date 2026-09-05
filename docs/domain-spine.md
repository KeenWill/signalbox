# Domain spine

This file is the maintainer's primary API-review surface. The crates are
authoritative: this is a hand-maintained mirror of their public API, updated
from source and never edited in source's place. The source files in
`crates/domain/src/` and `crates/application/src/` are intentionally dense with
rustdoc, unit tests, and `compile_fail` proofs; domain shape is reviewed here
instead. The mirror covers the public type and function surface of
`signalbox-domain` and `signalbox-application` as bare declarations — no doc
comments, no tests, no bodies. Any pull request that adds, removes, or changes a
public item in either crate must update this file in the same change;
`AGENTS.md` carries that rule, and CI (`scripts/check_domain_spine.py`) fails
when an exported name or a listed type's public method is missing here, when a
declaration outlives its source counterpart, or when an inventory count
disagrees with source.

Conventions used below:

- The declarations are illustrative, not compilable Rust. In particular,
  `pub struct Name { /* private */ }` marks a struct whose real fields are
  private — it is not a fieldless struct. Resolve exact field shapes and
  accessor return types in source.
- Enums are shown with their full variant lists — the variants are the semantic
  content.
- Structs have private fields unless declared as unit structs (a unit struct
  such as `UuidV7SessionIdGenerator;` is directly constructible). Structs a
  caller can build show their public constructors as full signatures; structs
  with no public constructor appear with a `// sealed:` comment naming the only
  public producer(s), or noting that the trusted producer is deferred to a later
  slice.
- Pure getters are collapsed to one `// accessors:` line per type.
- Public constructors, transitions, and `into_parts`-style decompositions are
  spelled out as bodiless `pub fn` signatures.
- Derives and trait implementations appear only where load-bearing (`Copy`
  versus non-`Copy`, equality composition, error traits); adding or removing one
  on a public type is a public-API change — update the relevant note when it
  matters, and treat source as the complete record.
- Comments state API shape only — sealed producers, crate-private seams,
  equality composition. Decided semantics live in the
  [living specification](spec/README.md) and are not restated here.

## domain: lib.rs — identities

Every identity is a UUID-backed newtype produced by one macro, with this common
shape (private field,
`Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd`):

```rust
pub struct <Identity>(/* private uuid::Uuid */);

impl <Identity> {
    pub const fn from_uuid(value: uuid::Uuid) -> Self;
    pub const fn as_uuid(&self) -> &uuid::Uuid;
    pub const fn into_uuid(self) -> uuid::Uuid;
}
```

The thirty identities defined in `lib.rs`:

```rust
pub struct DurableCommandId(/* private */);
pub struct SessionId(/* private */);
pub struct DelegationMessageId(/* private */);
pub struct ImportedConversationId(/* private */);
pub struct ImportedTranscriptEntryId(/* private */);
pub struct AcceptedInputId(/* private */);
pub struct TurnId(/* private */);
pub struct TurnAttemptId(/* private */);
pub struct ModelCallId(/* private */);
pub struct BlobDerivationId(/* private */);
pub struct ProviderTargetEvidenceId(/* private */);
pub struct ToolRequestId(/* private */);
pub struct ToolAttemptId(/* private */);
pub struct RunnerEnrollmentId(/* private */);
pub struct RunnerId(/* private */);
pub struct RunnerAuthenticationId(/* private */);
pub struct RunnerLeaseId(/* private */);
pub struct WorkspaceManifestId(/* private */);
pub struct ProgramRunId(/* private */);
pub struct ReviewTargetId(/* private */);
pub struct ReviewRunId(/* private */);
pub struct ReviewPassId(/* private */);
pub struct ReviewFindingId(/* private */);
pub struct ReviewExternalLinkId(/* private */);
pub struct RepoWatchEventId(/* private */);
pub struct RepoWatchDispatchId(/* private */);
pub struct CommissionedDispatchId(/* private */);
pub struct WorkspaceId(/* private */);
pub struct GitRemoteMintId(/* private */);
pub struct GitRemoteWithdrawalId(/* private */);
```

Six more identities with the same shape are defined in their owning modules and
listed there: `DirectModelSelection`, `ModelAlias` (configuration),
`ProviderModelIdentity` (model_call), `ContextFrontierId`,
`SemanticTranscriptEntryId` (context_frontier), and `ContextCompactionId`
(context_compaction).

## domain: actor

```rust
pub enum Actor {
    User,
    Core,
    Model { turn: TurnId },
    Recovery,
    Tool { request: ToolRequestId },
}
```

## domain: blob

```rust
pub struct BlobDigest(/* private [u8; 32] */);
impl BlobDigest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self;
    pub const fn as_bytes(&self) -> &[u8; 32];
    pub fn digest(bytes: &[u8]) -> Self;
}

pub enum BlobDigestParseFailure {
    MissingSha256Prefix,
    InvalidLength,
    InvalidHex,
}

pub struct BlobDigestParseError { /* private */ }
impl BlobDigestParseError {
    // accessors: rejected(), failure()
}

pub struct BlobTransformationName(/* private */);
impl BlobTransformationName {
    pub fn try_new(value: impl Into<Arc<str>>) -> Result<Self, BlobTransformationError>;
    // accessor: as_str()
}

pub struct BlobTransformation { /* private */ }
impl BlobTransformation {
    pub fn try_new(
        name: BlobTransformationName,
        version: u32,
        parameters: &serde_json::Value,
    ) -> Result<Self, BlobTransformationError>;
    // accessors: name(), version(), parameters_json()
}

pub enum BlobTransformationError {
    InvalidName,
    ZeroVersion,
    InvalidParameters,
    ParametersTooLarge,
}

pub enum BlobDerivationProducer {
    Deterministic { implementation: BlobDigest },
    Executed { execution_id: uuid::Uuid, implementation: BlobDigest },
    ModelDerived { model_call: ModelCallId },
}

pub struct DeterministicBlobDerivationKey(/* private BlobDigest */);
impl DeterministicBlobDerivationKey {
    pub fn try_derive(
        inputs: &[BlobDigest],
        transformation: &BlobTransformation,
        implementation: BlobDigest,
    ) -> Result<Self, BlobDerivationError>;
    // accessor: digest()
}

pub struct BlobDerivation { /* private */ }
impl BlobDerivation {
    pub fn try_new(
        id: BlobDerivationId,
        inputs: impl Into<Box<[BlobDigest]>>,
        transformation: BlobTransformation,
        producer: BlobDerivationProducer,
        outputs: impl Into<Box<[BlobDigest]>>,
    ) -> Result<Self, BlobDerivationError>;
    // accessors: id(), inputs(), transformation(), producer(), outputs(),
    // deterministic_key()
}

pub enum BlobDerivationError {
    EmptyInputs,
    TooManyInputs,
    EmptyOutputs,
    TooManyOutputs,
}
```

## domain: program_journal

```rust
pub struct JournalPosition(/* private NonZeroU64 */);
pub struct RequestOrdinal(/* private NonZeroU64 */);
pub struct DeliveryOrdinal(/* private NonZeroU64 */);
pub struct ScopeOrdinal(/* private NonZeroU64 */);

pub struct InlineFramePayload { /* private */ }
impl InlineFramePayload {
    pub fn new(bytes: impl Into<Box<[u8]>>) -> Self;
    // accessor: as_bytes()
}

pub enum ProgramCapability {
    Time,
    Random,
    Sleep,
    Subscribe,
    Session,
    Judge,
    ExecStage,
    Corpus,
    EvalRecord,
    Blob,
    Register,
}

pub enum ScopeOperation {
    Open,
    Close,
}

pub struct ScopeRequest { /* private */ }
impl ScopeRequest {
    pub const fn new(
        operation: ScopeOperation,
        scope: ScopeOrdinal,
        parent: Option<ScopeOrdinal>,
    ) -> Self;
    // accessors: operation(), scope(), parent()
}

pub struct EffectRequest { /* private */ }
impl EffectRequest {
    pub fn new(
        capability: ProgramCapability,
        method: String,
        payload: InlineFramePayload,
    ) -> Self;
    // accessors: capability(), method(), payload()
}

pub enum RequestKind {
    Now(InlineFramePayload),
    Random(InlineFramePayload),
    Sleep(InlineFramePayload),
    AwaitEvent(InlineFramePayload),
    Effect(EffectRequest),
    Scope(ScopeRequest),
    Terminal(InlineFramePayload),
}

pub struct RequestFrame { /* private */ }
impl RequestFrame {
    pub const fn new(
        ordinal: RequestOrdinal,
        scope: Option<ScopeOrdinal>,
        kind: RequestKind,
    ) -> Self;
    // accessors: ordinal(), scope(), kind()
}

pub enum RejectReason {
    OutstandingRequests,
}

pub enum FaultCause {
    Timeout,
    Memory,
    Nondeterminism,
    ProgramError,
    ContractRetired,
    JournalBound,
    PayloadTooLarge,
}

pub enum ProgramFault {
    Timeout(InlineFramePayload),
    Memory(InlineFramePayload),
    Nondeterminism {
        expected: RequestFrame,
        observed: RequestFrame,
    },
    ProgramError(InlineFramePayload),
    ContractRetired(InlineFramePayload),
    JournalBound(InlineFramePayload),
    PayloadTooLarge(InlineFramePayload),
}
impl ProgramFault {
    // accessors: cause(), evidence()
}

pub enum FaultEvidenceRef<'a> {
    Ordinary(&'a InlineFramePayload),
    Nondeterminism {
        expected: &'a RequestFrame,
        observed: &'a RequestFrame,
    },
}

pub enum DeliveryKind {
    Answer {
        resolves: RequestOrdinal,
        payload: InlineFramePayload,
    },
    Wake {
        resolves: RequestOrdinal,
        payload: InlineFramePayload,
    },
    Reject {
        resolves: RequestOrdinal,
        reason: RejectReason,
    },
    Cancel {
        resolves: RequestOrdinal,
        payload: InlineFramePayload,
    },
    RunCancel(InlineFramePayload),
    Fault(ProgramFault),
}
impl DeliveryKind {
    // accessor: resolves()
}

pub struct DeliveryFrame { /* private */ }
impl DeliveryFrame {
    pub const fn new(ordinal: DeliveryOrdinal, kind: DeliveryKind) -> Self;
    // accessors: ordinal(), kind()
}

pub enum JournalFrame {
    Request(RequestFrame),
    Delivery(DeliveryFrame),
}

pub struct JournalEntry { /* private */ }
impl JournalEntry {
    pub const fn new(position: JournalPosition, frame: JournalFrame) -> Self;
    // accessors: position(), frame()
}

pub struct ProgramJournal { /* private */ }
impl ProgramJournal {
    pub fn try_new(
        run: ProgramRunId,
        entries: Vec<JournalEntry>,
    ) -> Result<Self, ProgramJournalError>;
    pub fn terminal_delivery(&self) -> Option<&DeliveryFrame>;
    // accessors: run(), entries()
}

pub enum ProgramJournalError {
    NoncontiguousPosition,
    NoncontiguousRequestOrdinal,
    NoncontiguousDeliveryOrdinal,
    UnknownResolvedRequest,
    RequestResolvedTwice,
    OrdinalExhausted,
}

pub enum ReplayInstruction {
    AwaitRequest,
    Deliver(DeliveryFrame),
    Live,
}

pub enum ReplayedRequest {
    Matched,
    DeliveryPending,
    Live,
}

pub struct NondeterminismError { /* private */ }
impl NondeterminismError {
    pub fn into_fault(self) -> ProgramFault;
    // accessors: run(), expected(), observed()
}

pub struct ReplayCursor { /* private */ }
impl ReplayCursor {
    pub fn new(journal: ProgramJournal) -> Self;
    pub fn next_instruction(&mut self) -> ReplayInstruction;
    pub fn submit_request(
        &mut self,
        observed: RequestFrame,
    ) -> Result<ReplayedRequest, NondeterminismError>;
}
```

## domain: imported_conversation

```rust
pub enum ImportedConversationFormat {
    ClaudeCodeSessionJsonlV1,
    ClaudeCodeSessionJsonlV2,
    CodexRolloutJsonlV1,
}

pub struct ImportedRawRecordHash(/* private [u8; 32] */);
impl ImportedRawRecordHash {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self;
    pub const fn as_bytes(&self) -> &[u8; 32];
    pub fn digest(bytes: &[u8]) -> Self;
}

pub struct ImportedRawRecordConversionDigest(/* private [u8; 32] */);
impl ImportedRawRecordConversionDigest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self;
    pub const fn as_bytes(&self) -> &[u8; 32];
}

pub struct ImportedConversationSourceDigest(/* private [u8; 32] */);
impl ImportedConversationSourceDigest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self;
    pub const fn as_bytes(&self) -> &[u8; 32];
}

pub enum ImportedSourceAttestation<Value> {
    Attested(Value),
    AttestedAbsent,
    NotAttested,
}

pub struct ImportedText(/* private String */);
impl ImportedText {
    pub fn new(value: String) -> Self;
    pub fn as_str(&self) -> &str;
    pub fn into_string(self) -> String;
}
// Debug is content-redacted.

pub struct ImportedJsonNumber(/* private String */);
impl ImportedJsonNumber {
    pub fn try_new(value: String) -> Result<Self, ImportedJsonNumberError>;
    pub fn as_str(&self) -> &str;
    pub fn into_string(self) -> String;
}
// Debug is content-redacted.

pub struct ImportedJsonNumberError { /* private */ }
impl ImportedJsonNumberError {
    pub fn value(&self) -> &str;
    pub fn into_value(self) -> String;
}
// Debug is content-redacted; implements Error.

pub struct ImportedStructuredObjectMember { /* private */ }
impl ImportedStructuredObjectMember {
    pub fn new(name: ImportedText, value: ImportedStructuredValue) -> Self;
    // accessors: name(), value()
}

pub enum ImportedStructuredValue {
    Null,
    Boolean(bool),
    Number(ImportedJsonNumber),
    String(ImportedText),
    Array(Box<[ImportedStructuredValue]>),
    Object(Box<[ImportedStructuredObjectMember]>),
}

pub struct ImportedStructuredFieldError;
// implements Error; content-silent.

pub fn unique_imported_structured_field<'members>(
    members: &'members [ImportedStructuredObjectMember],
    name: &str,
) -> Result<
    Option<&'members ImportedStructuredValue>,
    ImportedStructuredFieldError,
>;

pub fn imported_text_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<ImportedText>, ImportedStructuredFieldError>;

pub fn imported_bool_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<bool>, ImportedStructuredFieldError>;

pub fn imported_structured_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<
    ImportedSourceAttestation<ImportedStructuredValue>,
    ImportedStructuredFieldError,
>;

pub fn imported_string_structured_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<
    ImportedSourceAttestation<ImportedStructuredValue>,
    ImportedStructuredFieldError,
>;

pub enum ImportedSpeaker {
    User,
    Assistant,
}

pub struct ImportedSourceMetadata { /* private */ }
impl ImportedSourceMetadata {
    pub const fn new(
        record_id: ImportedSourceAttestation<ImportedText>,
        parent_record_id: ImportedSourceAttestation<ImportedText>,
        source_session_id: ImportedSourceAttestation<ImportedText>,
        timestamp: ImportedSourceAttestation<ImportedText>,
        sidechain: ImportedSourceAttestation<bool>,
        metadata: ImportedSourceAttestation<bool>,
        message_role: ImportedSourceAttestation<ImportedSpeaker>,
    ) -> Self;
    // accessors: record_id(), parent_record_id(), source_session_id(),
    //   timestamp(), sidechain(), metadata(), message_role()
}

pub enum ImportedMessageContentAbsence {
    MessageNotAttested,
    MessageAttestedAbsent,
    ContentNotAttested,
    ContentAttestedAbsent,
    EmptyBlockArray,
}

pub struct ImportedMediaSource { /* private */ }
impl ImportedMediaSource {
    pub const fn new(
        kind: ImportedSourceAttestation<ImportedText>,
        media_type: ImportedSourceAttestation<ImportedText>,
        data: ImportedSourceAttestation<ImportedText>,
    ) -> Self;
    // accessors: kind(), media_type(), data()
}

pub enum ImportedToolResultBlock {
    Text(ImportedSourceAttestation<ImportedText>),
    Image(ImportedSourceAttestation<ImportedMediaSource>),
    ToolReference {
        tool_name: ImportedSourceAttestation<ImportedText>,
    },
    SourceResultBlock {
        source_type: ImportedSourceAttestation<ImportedText>,
    },
}

pub enum ImportedToolResultValue {
    Text(ImportedText),
    Blocks(Box<[ImportedToolResultBlock]>),
}

pub enum ImportedTranscriptContent {
    SourceEvent {
        source_type: ImportedSourceAttestation<ImportedText>,
    },
    SourceMessageBlock {
        source_type: ImportedSourceAttestation<ImportedText>,
    },
    Text(ImportedSourceAttestation<ImportedText>),
    ToolCall {
        source_call_id: ImportedSourceAttestation<ImportedText>,
        name: ImportedSourceAttestation<ImportedText>,
        input: ImportedSourceAttestation<ImportedStructuredValue>,
        caller: ImportedSourceAttestation<ImportedStructuredValue>,
    },
    ToolResult {
        source_call_id: ImportedSourceAttestation<ImportedText>,
        content: ImportedSourceAttestation<ImportedToolResultValue>,
        is_error: ImportedSourceAttestation<bool>,
    },
    Thinking {
        thinking: ImportedSourceAttestation<ImportedText>,
        signature: ImportedSourceAttestation<ImportedText>,
    },
    RedactedThinking {
        data: ImportedSourceAttestation<ImportedText>,
    },
    Document {
        source: ImportedSourceAttestation<ImportedMediaSource>,
    },
    MessageContentAbsent(ImportedMessageContentAbsence),
}

pub struct ImportedRawRecordPosition(/* private positive u64 */);
pub struct ImportedRecordEntryPosition(/* private positive u64 */);
pub struct ImportedTranscriptPosition(/* private */);
// Each position type has this common API:
impl <Position> {
    pub const fn try_from_u64(value: u64) -> Option<Self>;
    pub const fn as_u64(self) -> u64;
    pub const fn first() -> Self;
    pub const fn checked_next(self) -> Option<Self>;
}

pub struct ImportedRawSourceRecord { /* private */ }
impl ImportedRawSourceRecord {
    pub fn from_converted(
        bytes: Vec<u8>,
        normalized: ImportedStructuredValue,
    ) -> Self;
    // accessors: content_hash(), conversion_digest(), bytes(), normalized()
}
// Debug redacts bytes and normalized content.

pub struct ImportedRawSourceRecordReconstitutionInput { /* private */ }
impl ImportedRawSourceRecordReconstitutionInput {
    pub fn new(
        position: ImportedRawRecordPosition,
        stored_hash: ImportedRawRecordHash,
        stored_conversion_digest: ImportedRawRecordConversionDigest,
        bytes: Vec<u8>,
        normalized: ImportedStructuredValue,
    ) -> Self;
    // accessors: position(), stored_hash(), stored_conversion_digest(), bytes(),
    //   normalized()
}
// Debug redacts bytes and normalized content.

pub struct ImportedTranscriptEntryInput { /* private */ }
impl ImportedTranscriptEntryInput {
    pub const fn new(
        identity: ImportedTranscriptEntryId,
        conversation: ImportedConversationId,
        position: ImportedTranscriptPosition,
        raw_record_position: ImportedRawRecordPosition,
        record_entry_position: ImportedRecordEntryPosition,
        source_speaker: ImportedSourceAttestation<ImportedSpeaker>,
        content: ImportedTranscriptContent,
        source: ImportedSourceMetadata,
    ) -> Self;
    // accessors: identity(), conversation(), position(), raw_record_position(),
    //   record_entry_position(), source_speaker(), content(), source()
}

pub struct ImportedTranscriptEntry { /* private */ }
// sealed: ImportedConversation::from_converted_records or
// ImportedConversationReconstitutionInput::reconstitute
impl ImportedTranscriptEntry {
    // accessors: identity(), conversation(), position(), raw_record_position(),
    //   record_entry_position(), source_speaker(), content(), source()
}

pub struct ImportedTranscriptFrontier { /* private */ }
// Copy; equality is the exact imported-conversation boundary.
impl ImportedTranscriptFrontier {
    pub const fn from_parts(
        conversation: ImportedConversationId,
        through_entry: ImportedTranscriptEntryId,
        through_position: ImportedTranscriptPosition,
    ) -> Self;
    // accessors: conversation(), through_entry(), through_position()
}

pub struct ImportedConversationReconstitutionInput { /* private */ }
impl ImportedConversationReconstitutionInput {
    pub fn new(
        requested_conversation: ImportedConversationId,
        stored_conversation: ImportedConversationId,
        format: ImportedConversationFormat,
        stored_source_digest: ImportedConversationSourceDigest,
        declared_raw_record_count: u64,
        raw_records: Vec<ImportedRawSourceRecordReconstitutionInput>,
        declared_entry_count: u64,
        entries: Vec<ImportedTranscriptEntryInput>,
    ) -> Self;
    pub fn reconstitute(self)
        -> Result<ImportedConversation, ImportedConversationReconstitutionError>;
    // accessors: requested_conversation(), stored_conversation(), format(),
    //   stored_source_digest(), declared_raw_record_count(), raw_records(),
    //   declared_entry_count(), entries()
}

pub enum ImportedConversationReconstitutionFailure {
    RequestedConversationMismatch,
    EmptyRawRecords,
    EmptyEntries,
    DeclaredRawRecordCountMismatch {
        declared: u64,
        actual: usize,
    },
    DeclaredEntryCountMismatch {
        declared: u64,
        actual: usize,
    },
    RawRecordPositionMismatch {
        expected: ImportedRawRecordPosition,
        actual: ImportedRawRecordPosition,
    },
    RawRecordHashMismatch {
        position: ImportedRawRecordPosition,
    },
    EmptyRawRecord {
        position: ImportedRawRecordPosition,
    },
    RawRecordHashCollision {
        position: ImportedRawRecordPosition,
    },
    RawRecordConversionDigestMismatch {
        position: ImportedRawRecordPosition,
    },
    RawRecordNormalizedValueNotObject {
        position: ImportedRawRecordPosition,
    },
    RawRecordStructuredValueDepthExceeded {
        position: ImportedRawRecordPosition,
    },
    RawRecordProjectionInvalid {
        position: ImportedRawRecordPosition,
    },
    SourceDigestMismatch {
        expected: ImportedConversationSourceDigest,
        actual: ImportedConversationSourceDigest,
    },
    EntryConversationMismatch {
        entry: ImportedTranscriptEntryId,
    },
    EntryPositionMismatch {
        entry: ImportedTranscriptEntryId,
        expected: ImportedTranscriptPosition,
        actual: ImportedTranscriptPosition,
    },
    DuplicateEntry {
        entry: ImportedTranscriptEntryId,
    },
    EntryRawRecordPositionMismatch {
        entry: ImportedTranscriptEntryId,
        expected: ImportedRawRecordPosition,
        actual: ImportedRawRecordPosition,
    },
    EntryRawRecordNotFound {
        entry: ImportedTranscriptEntryId,
        position: ImportedRawRecordPosition,
    },
    EntryWithinRecordPositionMismatch {
        entry: ImportedTranscriptEntryId,
        expected: ImportedRecordEntryPosition,
        actual: ImportedRecordEntryPosition,
    },
    RawRecordWithoutEntry {
        position: ImportedRawRecordPosition,
    },
    SourceEventSpeakerMismatch {
        entry: ImportedTranscriptEntryId,
    },
    SourceRecordTypeMismatch {
        entry: ImportedTranscriptEntryId,
    },
    MessageSpeakerUnavailable {
        entry: ImportedTranscriptEntryId,
    },
    MessageRoleMismatch {
        entry: ImportedTranscriptEntryId,
    },
    EntryProjectionMismatch {
        entry: ImportedTranscriptEntryId,
    },
    RawRecordEntryProjectionMismatch {
        position: ImportedRawRecordPosition,
    },
    EntryStructuredValueDepthExceeded {
        entry: ImportedTranscriptEntryId,
    },
    PositionExhausted,
}

pub struct ImportedConversationReconstitutionError { /* private */ }
// sealed: Err of ImportedConversation::from_converted_records or
// ImportedConversationReconstitutionInput::reconstitute
impl ImportedConversationReconstitutionError {
    pub fn into_parts(
        self,
    ) -> (
        ImportedConversationReconstitutionInput,
        ImportedConversationReconstitutionFailure,
    );
    // accessors: failure(), input()
}

pub struct ImportedConversation { /* private */ }
// sealed: from_converted_records or checked reconstitution
impl ImportedConversation {
    pub fn from_converted_records(
        id: ImportedConversationId,
        format: ImportedConversationFormat,
        raw_records: Vec<ImportedRawSourceRecord>,
        entries: Vec<ImportedTranscriptEntryInput>,
    ) -> Result<Self, ImportedConversationReconstitutionError>;
    pub fn frontiers(&self) -> impl Iterator<Item = ImportedTranscriptFrontier> + '_;
    pub fn frontier_for_entry(
        &self,
        entry: ImportedTranscriptEntryId,
    ) -> Option<ImportedTranscriptFrontier>;
    pub fn prefix(
        &self,
        frontier: ImportedTranscriptFrontier,
    ) -> Option<&[ImportedTranscriptEntry]>;
    // accessors: id(), format(), source_digest(), raw_records(), entries()
}

pub struct ImportedConversationDisplayTitle(/* private String */);
impl ImportedConversationDisplayTitle {
    pub const MAX_SCALARS: usize;
    pub fn try_new(value: String) -> Result<Self, ImportedConversationDisplayTitleError>;
    pub fn derive(conversation: &ImportedConversation) -> Option<Self>;
    pub fn as_str(&self) -> &str;
    pub fn into_string(self) -> String;
}

pub enum ImportedConversationDisplayTitleError {
    Empty,
    ContainsNul,
    ContainsLineBreak,
    ExceedsMaxScalars { scalars: usize },
    UntrimmedEdgeWhitespace,
}
```

## domain: session_template

```rust
pub struct SessionTemplateName(/* private String */);
impl SessionTemplateName {
    pub const MAX_UTF8_BYTES: usize;
    pub fn try_new(value: String) -> Result<Self, SessionTemplateNameError>;
    pub fn as_str(&self) -> &str;
    pub fn into_string(self) -> String;
}

pub enum SessionTemplateNameFailure {
    Empty,
    TooLong { bytes: usize },
    InvalidFirstByte,
    InvalidByte,
}

pub struct SessionTemplateNameError { /* private */ }
impl SessionTemplateNameError {
    pub fn value(&self) -> &str;
    pub const fn failure(&self) -> SessionTemplateNameFailure;
    pub fn into_parts(self) -> (String, SessionTemplateNameFailure);
}

pub struct SessionTemplateVersion(/* private u64 */);
impl SessionTemplateVersion {
    pub const fn try_from_u64(value: u64) -> Option<Self>;
    pub const fn as_u64(self) -> u64;
}

pub struct SessionTemplateContentDigest(/* private [u8; 32] */);
impl SessionTemplateContentDigest {
    pub fn derive(
        version: SessionTemplateVersion,
        defaults: &SessionConfigurationDefaults,
    ) -> Option<Self>;
    pub const fn from_bytes(bytes: [u8; 32]) -> Self;
    pub const fn as_bytes(&self) -> &[u8; 32];
}

pub struct SessionTemplateProvenance { /* private */ }
impl SessionTemplateProvenance {
    pub const fn new(
        name: SessionTemplateName,
        content_digest: SessionTemplateContentDigest,
    ) -> Self;
    // accessors: name(), content_digest()
}
```

## domain: session_placement

```rust
pub struct SessionPlacementPath { /* private */ }
impl SessionPlacementPath {
    pub const MAX_DEPTH: usize;
    pub const MAX_SEGMENT_BYTES: usize;
    pub fn try_new(value: String) -> Result<Self, SessionPlacementPathError>;
    // accessors: as_str(), depth()
}
pub enum SessionPlacementPathError {
    Empty,
    EmptySegment,
    MalformedSegment,
    SegmentTooLong,
    TooDeep,
}
// impl Display + std::error::Error

pub enum RootPlacementGlobalReadIntent { Acknowledged }
pub struct SessionPlacement { /* private */ }
impl SessionPlacement {
    pub const fn pathless() -> Self;
    pub fn scoped(path: SessionPlacementPath) -> Result<Self, SessionPlacementError>;
    pub fn root_global_read(
        path: SessionPlacementPath,
        intent: RootPlacementGlobalReadIntent,
    ) -> Result<Self, SessionPlacementError>;
    pub fn decide_cross_session_read(&self, target: &Self) -> SessionReadScopeDecision;
    // accessors: path(), records_root_global_read_intent()
}
pub enum SessionPlacementError {
    RootRequiresGlobalReadIntent,
    GlobalReadIntentRequiresRoot,
}
// impl Display + std::error::Error
pub struct SessionPlacementDirectory { /* private */ }
impl SessionPlacementDirectory {
    // accessors: as_str(), prefix(), is_root()
}
pub enum SessionReadScopeDecision {
    Allowed,
    Refused(SessionReadScopeRefusal),
}
pub struct SessionReadScopeRefusal { /* private */ }
impl SessionReadScopeRefusal {
    // accessors: requesting_directory(), reason()
}
pub enum SessionReadRefusalReason { OutsideRequestingDirectorySubtree }

pub struct SessionPlacementVersion { /* private positive u64 */ }
impl SessionPlacementVersion {
    pub const INITIAL: Self;
    pub const fn try_from_u64(value: u64) -> Option<Self>;
    // accessors: as_u64(), next()
}
pub struct VersionedSessionPlacement { /* private */ }
impl VersionedSessionPlacement {
    pub const fn initial(placement: SessionPlacement) -> Self;
    pub const fn reconstitute(
        version: SessionPlacementVersion,
        placement: SessionPlacement,
    ) -> Self;
    // accessors: version(), placement()
}
pub struct UpdateSessionPlacement { /* private */ }
impl UpdateSessionPlacement {
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        expected_version: SessionPlacementVersion,
        replacement: SessionPlacement,
    ) -> Self;
    // accessors: command_id(), session(), expected_version(), replacement()
}
// Eq/Hash exclude command_id (comparison-payload rule,
// spec/identity-and-commands.md)
pub enum SessionPlacementEventKind { Created, Updated }
pub struct SessionPlacementEvent { /* private */ }
impl SessionPlacementEvent {
    pub fn created(
        session: SessionId,
        placement: SessionPlacement,
        command_id: DurableCommandId,
    ) -> Self;
    pub fn updated(
        session: SessionId,
        prior_version: SessionPlacementVersion,
        placement: SessionPlacement,
        command_id: DurableCommandId,
    ) -> Option<Self>;
    // accessors: session(), kind(), placement(), prior_version(), command_id()
}
pub enum UpdateSessionPlacementResult {
    Applied(UpdateSessionPlacementApplied),
    Rejected(UpdateSessionPlacementRejection),
}
pub struct UpdateSessionPlacementApplied { /* private */ }
impl UpdateSessionPlacementApplied {
    pub fn try_new(
        command: &UpdateSessionPlacement,
        event: SessionPlacementEvent,
    ) -> Option<Self>;
    // accessors: event()
}
pub enum UpdateSessionPlacementRejectionKind {
    SessionNotFound,
    CurrentVersionMismatch,
    VersionExhausted,
}
pub struct UpdateSessionPlacementRejection { /* private */ }
impl UpdateSessionPlacementRejection {
    pub const fn session_not_found(command: &UpdateSessionPlacement) -> Self;
    pub const fn current_version_mismatch(
        command: &UpdateSessionPlacement,
        current: SessionPlacementVersion,
    ) -> Option<Self>;
    pub const fn version_exhausted(
        command: &UpdateSessionPlacement,
        current: SessionPlacementVersion,
    ) -> Option<Self>;
    // accessors: session(), expected_version(), current_version(), kind()
}
```

## domain: session

```rust
pub enum SessionCreationCause {
    Interactive,
    ModuleDispatched { dispatch: ModuleDispatch },
    Delegated { spawning_request: ToolRequestId },
}
impl SessionCreationCause {
    pub const fn default_ownership(&self) -> SessionOwnership;
    pub const fn default_finish_condition(&self) -> Option<FinishCondition>;
}

pub struct TranscriptFrontier { /* private */ }
// sealed: no public producer in this slice; the later semantic-history slice
// supplies the trusted frontier producer. Copy; equality is exact-boundary.

pub enum ImportedSessionRelationship {
    Resume,
    Fork,
}

pub enum TranscriptAncestry {
    None,
    SingleSource {
        source_session: SessionId,
        source_frontier: TranscriptFrontier,
    },
    ImportedConversation {
        source_frontier: ImportedTranscriptFrontier,
        relationship: ImportedSessionRelationship,
    },
}

pub struct SessionCreationProvenance { /* private */ }
impl SessionCreationProvenance {
    pub const fn new(cause: SessionCreationCause, ancestry: TranscriptAncestry) -> Self;
    pub const fn delegated(spawning_request: ToolRequestId) -> Self;
    pub const fn module_dispatched(dispatch: ModuleDispatch) -> Self;
    // accessors: cause(), ancestry()
}

pub struct CreateSession { /* private */ }
impl CreateSession {
    pub const fn new(
        command_id: DurableCommandId,
        provenance: SessionCreationProvenance,
        initial_configuration_defaults: SessionConfigurationDefaults,
    ) -> Self;
    pub const fn new_with_placement(
        command_id: DurableCommandId,
        provenance: SessionCreationProvenance,
        initial_configuration_defaults: SessionConfigurationDefaults,
        placement: SessionPlacement,
    ) -> Self;
    pub const fn new_from_template(
        command_id: DurableCommandId,
        provenance: SessionCreationProvenance,
        template_provenance: SessionTemplateProvenance,
        resolved_configuration_defaults: SessionConfigurationDefaults,
    ) -> Self;
    pub const fn new_from_template_with_placement(
        command_id: DurableCommandId,
        provenance: SessionCreationProvenance,
        template_provenance: SessionTemplateProvenance,
        resolved_configuration_defaults: SessionConfigurationDefaults,
        placement: SessionPlacement,
    ) -> Self;
    pub fn with_lifecycle(
        self,
        start_gate: StartGate,
        ownership: SessionOwnership,
        finish_condition: Option<FinishCondition>,
    ) -> Self;
    pub fn establish_initial_defaults(&self) -> VersionedSessionConfigurationDefaults;
    pub fn prepare(self, session: SessionId)
        -> Result<PreparedCreateSession, CreateSessionPreparationError>;
    // accessors: command_id(), provenance(), initial_configuration_defaults(),
    //   template_provenance(), placement(), start_gate(), ownership(),
    //   finish_condition()
}
// Eq/Hash exclude command_id; explicit mode compares defaults, template mode
// compares the requested template name, and the two modes differ.

pub struct CreateSessionFromImportedFrontier { /* private */ }
impl CreateSessionFromImportedFrontier {
    pub const fn new(
        command_id: DurableCommandId,
        imported_frontier: ImportedTranscriptFrontier,
        relationship: ImportedSessionRelationship,
        initial_configuration_defaults: SessionConfigurationDefaults,
    ) -> Self;
    pub fn establish_initial_defaults(&self) -> VersionedSessionConfigurationDefaults;
    pub fn prepare<NextSemanticEntryId>(
        self,
        imported_conversation: &ImportedConversation,
        session: SessionId,
        seed_frontier: ContextFrontierId,
        next_semantic_entry_id: NextSemanticEntryId,
    ) -> Result<
        PreparedCreateSessionFromImportedFrontier,
        CreateSessionFromImportedFrontierPreparationError,
    >
    where
        NextSemanticEntryId: FnMut() -> SemanticTranscriptEntryId;
    // accessors: command_id(), imported_conversation(), imported_frontier(),
    //   relationship(), initial_configuration_defaults()
}
// Eq/Hash exclude command_id (comparison-payload rule,
// spec/identity-and-commands.md)

pub struct ImportedSessionSeed { /* private */ }
// sealed: checked imported-prefix preparation and reconstitution
impl ImportedSessionSeed {
    // accessors: session(), seed_frontier()
}

pub struct InitialSession { /* private */ }
// sealed: carried only by PreparedCreateSession,
// ReconstitutedSessionCreation, PreparedCreateSessionFromImportedFrontier,
// and ReconstitutedSessionCreationFromImportedFrontier
impl InitialSession {
    // accessors: id(), provenance(), template_provenance(),
    //   configuration_defaults(), placement()
}

pub struct Session { /* private */ }
// sealed: SessionReconstitutionInput::reconstitute,
// BoundedImportedSessionReconstitutionInput::reconstitute, or
// ReconstitutedImportedSession::into_parts
// non-Copy: owned snapshot, cloned deliberately (session aggregate,
// spec/sessions-and-transcript.md)
impl Session {
    // accessors: id(), creation_provenance(), template_provenance(),
    //   current_configuration_defaults(), current_placement()
}

pub struct SessionPlacementReconstitutionFacts {
    pub current_pointer_session: SessionId,
    pub current_pointer_version: SessionPlacementVersion,
    pub selected_event_session: SessionId,
    pub selected_event: VersionedSessionPlacement,
}

pub struct SessionReconstitutionInput { /* private */ }
impl SessionReconstitutionInput {
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
    ) -> Self;
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
    ) -> Self;
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
    ) -> Self;
    pub fn reconstitute(self) -> Result<Session, SessionReconstitutionError>;
    // accessors: requested_session(), stored_session(), provenance(),
    //   template_provenance(), current_defaults_session(), current_defaults_version(),
    //   defaults_session(), defaults_version(), defaults(), current_placement_session(),
    //   current_placement_version(), placement_session(), current_placement()
}

pub enum SessionReconstitutionFailure {
    RequestedSessionMismatch,
    CurrentDefaultsSessionMismatch,
    DefaultsSessionMismatch,
    CurrentDefaultsVersionMismatch,
    CurrentPlacementSessionMismatch,
    PlacementSessionMismatch,
    CurrentPlacementVersionMismatch,
    ImportedSessionSeedUnavailable,
    DelegatedAncestryMismatch,
    DelegatedTemplateProvenance,
}

pub struct SessionReconstitutionError { /* private */ }
// sealed: Err of SessionReconstitutionInput::reconstitute
impl SessionReconstitutionError {
    pub fn into_parts(self) -> (SessionReconstitutionInput, SessionReconstitutionFailure);
    // accessors: failure(), input()
}

pub struct CreateSessionAppliedResult { /* private */ }
// sealed: CreateSession::prepare and CreateSessionReconstitutionInput::reconstitute
impl CreateSessionAppliedResult {
    // accessors: session()
}

pub struct PreparedCreateSession { /* private */ }
// sealed: CreateSession::prepare
impl PreparedCreateSession {
    pub const fn into_parts(self)
        -> (CreateSession, InitialSession, CreateSessionAppliedResult);
    // accessors: command(), session(), applied_result()
}

pub enum CreateSessionPreparationFailure {
    TranscriptAncestryUnavailable,
    DelegatedCreationRequiresSpawn,
}

pub struct CreateSessionPreparationError { /* private */ }
// sealed: Err of CreateSession::prepare; not a terminal command rejection
impl CreateSessionPreparationError {
    pub fn into_parts(self) -> (SessionId, CreateSession, CreateSessionPreparationFailure);
    // accessors: failure(), command(), session()
}

pub struct CreateSessionReconstitutionInput { /* private */ }
impl CreateSessionReconstitutionInput {
    pub const fn new(
        command: CreateSession,
        result_session: SessionId,
        session: SessionId,
        provenance: SessionCreationProvenance,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    ) -> Self;
    pub const fn new_with_template_provenance(
        command: CreateSession,
        result_session: SessionId,
        session: SessionId,
        provenance: SessionCreationProvenance,
        template_provenance: Option<SessionTemplateProvenance>,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    ) -> Self;
    pub const fn new_with_template_and_placement(
        command: CreateSession,
        result_session: SessionId,
        session: SessionId,
        provenance: SessionCreationProvenance,
        template_provenance: Option<SessionTemplateProvenance>,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        placement: VersionedSessionPlacement,
    ) -> Self;
    pub fn reconstitute(self)
        -> Result<ReconstitutedSessionCreation, CreateSessionReconstitutionError>;
    // accessors: command(), result_session(), session(), provenance(),
    //   template_provenance(), defaults_session(), defaults_version(), defaults(), placement()
}

pub enum CreateSessionReconstitutionFailure {
    SessionResultMismatch,
    ProvenanceMismatch,
    TemplateProvenanceMismatch,
    PlacementMismatch,
    DefaultsSessionMismatch,
    TranscriptAncestryUnavailable,
    DelegatedCreationRequiresSpawn,
    DefaultsVersionIsNotFirst,
    DefaultsMismatch,
}

pub struct CreateSessionReconstitutionError { /* private */ }
// sealed: Err of CreateSessionReconstitutionInput::reconstitute
impl CreateSessionReconstitutionError {
    pub fn into_parts(self)
        -> (CreateSessionReconstitutionInput, CreateSessionReconstitutionFailure);
    // accessors: failure(), input()
}

pub struct ReconstitutedSessionCreation { /* private */ }
// sealed: CreateSessionReconstitutionInput::reconstitute; authorizes no effect
impl ReconstitutedSessionCreation {
    // accessors: command(), session(), applied_result()
}
```

## domain: session_delegation

```rust
pub const fn spawn_session_tool_name() -> &'static str;
pub const fn await_session_tool_name() -> &'static str;
pub const fn send_session_message_tool_name() -> &'static str;
pub enum BoundChildAction { KeepRunning, Stop, Cancel }
pub enum ChildRelationshipPolicy {
    Background,
    Bound {
        on_parent_stopped: BoundChildAction,
        on_parent_cancelled: BoundChildAction,
    },
}
pub enum DelegationWaitMode { Foreground, Background }
pub enum DescendantTerminationScope { ParentAlone, ParentAndDescendants }
pub enum ParentTerminationKind { Stopped, Cancelled }
pub enum ParentTerminationCommandSource {
    Turn { turn: TurnId },
    Goal { generation: GoalGeneration },
    Lifecycle,
}
pub struct ParentTerminationAuthority { /* private applied command authority */ }
// sealed: exact applied parent-termination producer deferred to scheduling
impl ParentTerminationAuthority {
    // accessors: parent(), source(), turn(), goal_generation(), command(), scope(), kind()
}

pub struct DelegationContent(/* private NonEmptyUnicodeText */);
impl DelegationContent {
    pub const MAX_UTF8_BYTES: usize;
    pub fn try_new(value: String) -> Result<Self, DelegationContentError>;
    pub fn as_str(&self) -> &str;
    pub fn from_assistant_text(parts: &[AssistantText]) -> Result<Self, DelegationContentError>;
}
pub enum DelegationContentFailure {
    Invalid(NonEmptyUnicodeTextFailure),
    Oversized { utf8_byte_length: usize },
}
pub struct DelegationContentError { /* private rejected String + failure */ }
impl DelegationContentError {
    pub fn into_parts(self) -> (String, DelegationContentFailure);
    // accessors: value(), failure()
}
pub enum DelegationRequestFailure {
    InvalidToolRequestPurpose,
    InvalidContent(DelegationContentError),
}
pub struct DelegationRequestError { /* private unchanged ToolRequest + failure; Error::source forwards invalid content */ }
impl DelegationRequestError {
    pub fn into_request(self) -> ToolRequest;
    // accessors: request(), failure()
}
pub struct DelegatedSpawnRequest { /* private canonical request + task + policy */ }
impl DelegatedSpawnRequest {
    pub fn parse(request: ToolRequest, task: String, policy: ChildRelationshipPolicy)
        -> Result<Self, DelegationRequestError>;
    // accessors: request(), task(), policy()
}
pub struct DelegationAwaitRequest { /* private canonical request + child + mode */ }
impl DelegationAwaitRequest {
    pub fn parse(request: ToolRequest, child: SessionId, mode: DelegationWaitMode)
        -> Result<Self, DelegationRequestError>;
    // accessors: request(), child(), mode()
}
pub struct DelegationMessageRequest { /* private canonical request + peer + content */ }
impl DelegationMessageRequest {
    pub fn parse(request: ToolRequest, peer: SessionId, content: String)
        -> Result<Self, DelegationRequestError>;
    // accessors: request(), peer(), content()
}

pub struct TerminalChildTurn { /* private checked terminal scheduling evidence, exact reason, and result digest */ }
impl TerminalChildTurn {
    pub fn from_completed(value: &CompletedModelCallTurn) -> Option<Self>;
    pub const fn from_failed(value: &FailedModelCallTurn) -> Self;
    pub const fn from_cancelled(value: &CancelledModelCallTurn) -> Self;
    pub const fn from_cancelled_tool_round(value: &CancelledToolRoundModelCallTurn) -> Self;
    pub const fn from_refused(value: &RefusedModelCallTurn) -> Self;
    pub const fn from_reconciliation_required(
        value: &ReconciliationRequiredModelCallTurn,
    ) -> Self;
    // accessors: session(), turn(), reason()
}

pub struct DelegationProvenance { /* private typed authority */ }
pub enum DelegationProvenanceProjection {
    ToolRequest {
        source_session: SessionId,
        source_turn: TurnId,
        request: ToolRequestId,
    },
    ChildTurn { terminal: TerminalChildTurn },
    ParentCommand { authority: ParentTerminationAuthority },
}
impl DelegationProvenance {
    pub fn from_spawn(request: &DelegatedSpawnRequest) -> Self;
    pub fn from_await(request: &DelegationAwaitRequest) -> Self;
    pub fn from_message(request: &DelegationMessageRequest) -> Self;
    pub const fn from_terminal_child(terminal: TerminalChildTurn) -> Self;
    pub const fn from_parent_termination(authority: ParentTerminationAuthority) -> Self;
    pub const fn projection(self) -> DelegationProvenanceProjection;
    // accessors: tool_request(), child_turn(), parent_command() returning the sealed authority
}

pub enum DelegationProvenanceReconstitutionInput {
    ChildTurn { session: SessionId, turn: TurnId },
    ParentTurnCommand {
        session: SessionId,
        turn: TurnId,
        command: DurableCommandId,
    },
    ParentGoalCommand {
        session: SessionId,
        generation: GoalGeneration,
        command: DurableCommandId,
    },
    ParentLifecycleCommand {
        session: SessionId,
        command: DurableCommandId,
    },
}

pub enum DelegationMessageDirection { ParentToChild, ChildToParent }
pub struct DelegationMessageEndpoints {
    pub parent: SessionId,
    pub child: SessionId,
}
pub struct DelegationMessage { /* private */ }
// sealed: SessionDelegation::deliver_message or DelegationMessage::reconstitute
impl DelegationMessage {
    pub fn reconstitute(
        request: &DelegationMessageRequest,
        id: DelegationMessageId,
        direction: DelegationMessageDirection,
        endpoints: DelegationMessageEndpoints,
    ) -> Option<Self>;
    // accessors: id(), direction(), content(), provenance()
}
pub enum DelegationOutcomeReason {
    ChildCompleted,
    ChildExecutionFailed,
    ChildResultUnavailable,
    ChildCancelled,
    ParentStopped { scope: DescendantTerminationScope },
    ParentCancelled { scope: DescendantTerminationScope },
}
pub enum DelegationOutcomeKind {
    ResultReturned,
    ChildFailed,
    ChildStopped,
    ChildCancelled,
    AlreadyTerminal,
    ContinueRunning,
}
pub struct DelegationOutcome { /* private validated kind + content + reason + provenance */ }
impl DelegationOutcome {
    pub fn from_completed_child(value: &CompletedModelCallTurn) -> Self;
    pub fn from_failed_child(value: &FailedModelCallTurn) -> Self;
    pub fn from_refused_child(value: &RefusedModelCallTurn) -> Self;
    pub fn from_cancelled_child(value: &CancelledModelCallTurn) -> Self;
    pub fn from_cancelled_tool_round_child(value: &CancelledToolRoundModelCallTurn) -> Self;
    pub fn from_reconciliation_required_child(
        value: &ReconciliationRequiredModelCallTurn,
    ) -> Self;
    pub fn from_terminal_child(terminal: TerminalChildTurn, content: Option<DelegationContent>)
        -> Option<Self>;
    pub fn reconstitute(
        kind: DelegationOutcomeKind,
        content: Option<DelegationContent>,
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenanceReconstitutionInput,
    ) -> Option<Self>;
    // accessors: kind(), content(), reason(), provenance(), reconstitution_provenance()
}
pub struct ChildWait { /* private awaiting request + spawning request + child */ }
// sealed: DelegationWait::foreground_subject
impl ChildWait {
    // accessors: awaiting_request(), spawning_request(), child()
}
pub struct DelegationWait { /* private */ }
// sealed: SessionDelegation::register_wait or DelegationWait reconstitution
impl DelegationWait {
    pub fn reconstitute(
        relation: &SessionDelegation,
        awaiting_request: &DelegationAwaitRequest,
    ) -> Option<Self>;
    pub fn reconstitute_stored(
        awaiting_request: &DelegationAwaitRequest,
        spawning_request: ToolRequestId,
        parent: SessionId,
        child: SessionId,
        mode: DelegationWaitMode,
    ) -> Option<Self>;
    // accessors: awaiting_request(), spawning_request(), parent(), child(), mode(), foreground_subject()
}
pub struct DelegationEventOrdinal(/* private NonZeroU64 */);
impl DelegationEventOrdinal {
    pub const fn new(value: NonZeroU64) -> Self;
    pub const fn get(self) -> u64;
}
pub enum DelegationEvent {
    Spawned {
        ordinal: DelegationEventOrdinal,
        provenance: DelegationProvenance,
    },
    MessageDelivered {
        ordinal: DelegationEventOrdinal,
        message: DelegationMessage,
    },
    OutcomeRecorded {
        ordinal: DelegationEventOrdinal,
        outcome: DelegationOutcome,
    },
}
impl DelegationEvent {
    // accessors: ordinal(), message(), outcome()
}
pub enum DelegationLifecycle { Active, Terminal }
pub struct SessionDelegationReconstitutionInput { /* private complete stored relation */ }
impl SessionDelegationReconstitutionInput {
    pub fn new(
        spawning_request: DelegatedSpawnRequest,
        child: SessionId,
        child_turn: TurnId,
        events: Vec<DelegationEvent>,
    ) -> Self;
    pub fn reconstitute(
        self,
    ) -> Result<SessionDelegation, SessionDelegationReconstitutionError>;
    // accessors: spawning_request(), child(), child_turn(), events()
}
pub enum SessionDelegationReconstitutionFailure {
    SameSession,
    MissingSpawnEvent,
    NoncontiguousEventOrdinal,
    InvalidSpawnEvent,
    InvalidMessageProvenance,
    DuplicateMessageIdentity,
    DuplicateMessageRequest,
    DuplicateOutcomeAuthority,
    OutcomeReasonMismatch,
    EventAfterTerminal,
}
pub struct SessionDelegationReconstitutionError { /* private unchanged input + failure */ }
impl SessionDelegationReconstitutionError {
    pub fn into_parts(
        self,
    ) -> (
        SessionDelegationReconstitutionInput,
        SessionDelegationReconstitutionFailure,
    );
    // accessors: input(), failure()
}
pub struct SessionDelegation { /* private */ }
impl SessionDelegation {
    pub fn register_wait(
        &self,
        awaiting_request: &DelegationAwaitRequest,
        dispatch: &ToolDispatchAuthority,
    ) -> Result<DelegationWait, DelegationTransitionError>;
    pub fn deliver_message(
        self,
        sending_request: DelegationMessageRequest,
        id: DelegationMessageId,
        dispatch: &ToolDispatchAuthority,
    ) -> Result<(Self, DelegationEvent), DelegationTransitionError>;
    pub fn record_outcome(
        self,
        outcome: DelegationOutcome,
    ) -> Result<Self, DelegationTransitionError>;
    pub fn record_parent_termination(
        self,
        authority: ParentTerminationAuthority,
    ) -> Result<Self, DelegationTransitionError>;
    // accessors: spawning_request(), parent(), child(), child_turn(), task(), policy(),
    //   lifecycle(), events(), child_creation_provenance()
}
pub enum DelegationTransitionFailure {
    SameSession,
    AlreadyTerminal,
    MissingSpawnEvent,
    InvalidProvenance,
    DescendantsNotSelected,
    DuplicateMessageIdentity,
    ConflictingMessageReplay,
    DuplicateOutcomeAuthority,
    OutcomeReasonMismatch,
    EventOrdinalExhausted,
}
pub enum RejectedDelegationTransition {
    Spawn {
        request: DelegatedSpawnRequest,
        child: SessionId,
        child_turn: TurnId,
    },
    DeliverMessage {
        relation: SessionDelegation,
        request: DelegationMessageRequest,
        id: DelegationMessageId,
    },
    RecordOutcome {
        relation: SessionDelegation,
        outcome: DelegationOutcome,
    },
    RecordParentTermination {
        relation: SessionDelegation,
        authority: ParentTerminationAuthority,
    },
}
pub struct DelegationTransitionError { /* private unchanged consuming input */ }
impl DelegationTransitionError {
    pub fn into_rejected(self) -> Option<RejectedDelegationTransition>;
    // accessors: spawning_request(), failure()
}
```

## domain: imported_session

```rust
pub struct CreateSessionFromImportedFrontierAppliedResult { /* private */ }
// sealed: checked preparation or complete reconstitution
impl CreateSessionFromImportedFrontierAppliedResult {
    // accessors: session()
}

pub struct PreparedCreateSessionFromImportedFrontier { /* private */ }
// sealed: CreateSessionFromImportedFrontier::prepare
impl PreparedCreateSessionFromImportedFrontier {
    pub fn into_parts(
        self,
    ) -> (
        CreateSessionFromImportedFrontier,
        InitialSession,
        Box<[SemanticTranscriptEntry]>,
        ResolvedContextFrontierSnapshot,
        ImportedSessionSeed,
        CreateSessionFromImportedFrontierAppliedResult,
    );
    // accessors: command(), session(), semantic_entries(), seed_snapshot(),
    //   imported_seed(), applied_result()
}

pub enum CreateSessionFromImportedFrontierPreparationFailure {
    ImportedConversationMismatch,
    ImportedFrontierNotFound,
    DuplicateSemanticEntryIdentity { entry: SemanticTranscriptEntryId },
}

pub struct CreateSessionFromImportedFrontierPreparationError { /* private */ }
// sealed: Err of CreateSessionFromImportedFrontier::prepare
impl CreateSessionFromImportedFrontierPreparationError {
    pub fn into_parts(
        self,
    ) -> (
        CreateSessionFromImportedFrontier,
        SessionId,
        ContextFrontierId,
        CreateSessionFromImportedFrontierPreparationFailure,
    );
    // accessors: command(), session(), seed_frontier(), failure()
}

pub struct ImportedSessionSeedReconstitutionInput { /* private */ }
impl ImportedSessionSeedReconstitutionInput {
    pub const fn new(session: SessionId, seed_frontier: ContextFrontierId) -> Self;
    // accessors: session(), seed_frontier()
}

pub struct ImportedSessionSeedHeaderReconstitutionInput { /* private */ }
impl ImportedSessionSeedHeaderReconstitutionInput {
    pub const fn new(
        owning_session: SessionId,
        seed_frontier: ContextFrontierId,
        declared_member_count: u64,
    ) -> Self;
    // accessors: owning_session(), seed_frontier(), declared_member_count()
}

pub struct BoundedImportedSessionReconstitutionInput { /* private */ }
impl BoundedImportedSessionReconstitutionInput {
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
        seed_records: Vec<ImportedSessionSeedReconstitutionInput>,
        seed_headers: Vec<ImportedSessionSeedHeaderReconstitutionInput>,
    ) -> Self;
    pub fn from_stored_imported_parts(
        requested_session: SessionId,
        stored_session: SessionId,
        creation_cause: SessionCreationCause,
        imported_conversation: ImportedConversationId,
        imported_frontier_entry: ImportedTranscriptEntryId,
        imported_frontier_position: ImportedTranscriptPosition,
        imported_relationship: ImportedSessionRelationship,
        current_defaults_session: SessionId,
        current_defaults_version: SessionConfigurationDefaultsVersion,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        placement: SessionPlacementReconstitutionFacts,
        seed_records: Vec<ImportedSessionSeedReconstitutionInput>,
        seed_headers: Vec<ImportedSessionSeedHeaderReconstitutionInput>,
    ) -> Self;
    pub fn reconstitute(
        self,
    ) -> Result<Session, BoundedImportedSessionReconstitutionError>;
    // accessors: requested_session(), stored_session(), provenance(),
    //   current_defaults_session(), current_defaults_version(),
    //   defaults_session(), defaults_version(), defaults(), current_placement_session(),
    //   current_placement_version(), placement_session(), current_placement(),
    //   seed_records(), seed_headers()
}

pub enum BoundedImportedSessionReconstitutionFailure {
    RequestedSessionMismatch,
    CurrentDefaultsSessionMismatch,
    DefaultsSessionMismatch,
    CurrentDefaultsVersionMismatch,
    CurrentPlacementSessionMismatch,
    PlacementSessionMismatch,
    CurrentPlacementVersionMismatch,
    DelegatedAncestryMismatch,
    AncestryNotImported,
    MissingSeedRecord,
    DuplicateSeedRecord,
    SeedSessionMismatch,
    MissingSeedHeader,
    DuplicateSeedHeader,
    SeedHeaderSessionMismatch,
    SeedHeaderIdentityMismatch,
    SeedMemberCountMismatch,
}

pub struct BoundedImportedSessionReconstitutionError { /* private */ }
// sealed: Err of BoundedImportedSessionReconstitutionInput::reconstitute
impl BoundedImportedSessionReconstitutionError {
    pub fn into_parts(
        self,
    ) -> (
        BoundedImportedSessionReconstitutionInput,
        BoundedImportedSessionReconstitutionFailure,
    );
    // accessors: failure(), input()
}

pub enum ImportedSessionSeedReconstitutionFailure {
    AncestryNotImported,
    ImportedConversationMismatch,
    ImportedFrontierNotFound,
    MissingSeedRecord,
    DuplicateSeedRecord,
    SeedSessionMismatch,
    MissingSeedSnapshot,
    DuplicateSeedSnapshot,
    SeedSnapshotSessionMismatch,
    SeedSnapshotIdentityMismatch,
    SemanticEntryCountMismatch { expected: usize, actual: usize },
    SemanticEntrySourceSessionMismatch { entry: SemanticTranscriptEntryId },
    DuplicateSemanticEntry { entry: SemanticTranscriptEntryId },
    SemanticEntryNotImported { entry: SemanticTranscriptEntryId },
    ImportedEntryIdentityMismatch { entry: SemanticTranscriptEntryId },
    ImportedSpeakerMismatch { entry: SemanticTranscriptEntryId },
    ImportedContentMismatch { entry: SemanticTranscriptEntryId },
    SeedSnapshotMalformed,
    SeedSnapshotMembershipMismatch,
}

pub struct ImportedSessionReconstitutionInput { /* private */ }
impl ImportedSessionReconstitutionInput {
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
        imported_conversation: ImportedConversation,
        seed_records: Vec<ImportedSessionSeedReconstitutionInput>,
        seed_snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
        semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    ) -> Self;
    pub fn reconstitute(
        self,
    ) -> Result<ReconstitutedImportedSession, ImportedSessionReconstitutionError>;
    // accessors: requested_session(), stored_session(), provenance(),
    //   current_defaults_session(), current_defaults_version(),
    //   defaults_session(), defaults_version(), defaults(), current_placement_session(),
    //   current_placement_version(), placement_session(), current_placement(),
    //   imported_conversation(), seed_records(), seed_snapshots(),
    //   semantic_entries()
}

pub enum ImportedSessionReconstitutionFailure {
    RequestedSessionMismatch,
    CurrentDefaultsSessionMismatch,
    DefaultsSessionMismatch,
    CurrentDefaultsVersionMismatch,
    CurrentPlacementSessionMismatch,
    PlacementSessionMismatch,
    CurrentPlacementVersionMismatch,
    DelegatedAncestryMismatch,
    Seed(ImportedSessionSeedReconstitutionFailure),
}

pub struct ImportedSessionNormalizedReconstitutionInput { /* private */ }
impl ImportedSessionNormalizedReconstitutionInput {
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
        imported_entries: Vec<ImportedTranscriptEntryInput>,
        seed_records: Vec<ImportedSessionSeedReconstitutionInput>,
        seed_snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
        semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    ) -> Self;
    pub fn reconstitute(
        self,
    ) -> Result<
        ReconstitutedImportedSession,
        ImportedSessionNormalizedReconstitutionError,
    >;
}

pub struct ImportedSessionNormalizedReconstitutionError { /* private */ }
// sealed: Err of ImportedSessionNormalizedReconstitutionInput::reconstitute
impl ImportedSessionNormalizedReconstitutionError {
    pub fn into_parts(
        self,
    ) -> (
        ImportedSessionNormalizedReconstitutionInput,
        ImportedSessionReconstitutionFailure,
    );
    // accessors: failure(), input()
}

pub struct ImportedSessionReconstitutionError { /* private */ }
// sealed: Err of ImportedSessionReconstitutionInput::reconstitute
impl ImportedSessionReconstitutionError {
    pub fn into_parts(
        self,
    ) -> (
        ImportedSessionReconstitutionInput,
        ImportedSessionReconstitutionFailure,
    );
    // accessors: failure(), input()
}

pub struct ReconstitutedImportedSession { /* private */ }
// sealed: ImportedSessionReconstitutionInput::reconstitute
impl ReconstitutedImportedSession {
    pub fn into_parts(
        self,
    ) -> (
        Session,
        ImportedSessionSeed,
        ResolvedContextFrontierSnapshot,
        Box<[SemanticTranscriptEntry]>,
    );
    // accessors: session(), imported_seed(), seed_snapshot(),
    //   semantic_entries()
}

pub struct CreateSessionFromImportedFrontierReconstitutionInput { /* private */ }
impl CreateSessionFromImportedFrontierReconstitutionInput {
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
    ) -> Self;
    pub fn reconstitute(
        self,
    ) -> Result<
        ReconstitutedSessionCreationFromImportedFrontier,
        CreateSessionFromImportedFrontierReconstitutionError,
    >;
    // accessors: command(), result_session(), session(), provenance(),
    //   defaults_session(), defaults_version(), defaults(),
    //   imported_conversation(), seed_records(), seed_snapshots(),
    //   semantic_entries()
}

pub enum CreateSessionFromImportedFrontierReconstitutionFailure {
    SessionResultMismatch,
    ProvenanceMismatch,
    DefaultsSessionMismatch,
    DefaultsVersionIsNotFirst,
    DefaultsMismatch,
    Seed(ImportedSessionSeedReconstitutionFailure),
}

pub struct CreateSessionFromImportedFrontierReconstitutionError { /* private */ }
// sealed: Err of
// CreateSessionFromImportedFrontierReconstitutionInput::reconstitute
impl CreateSessionFromImportedFrontierReconstitutionError {
    pub fn into_parts(
        self,
    ) -> (
        CreateSessionFromImportedFrontierReconstitutionInput,
        CreateSessionFromImportedFrontierReconstitutionFailure,
    );
    // accessors: failure(), input()
}

pub struct ReconstitutedSessionCreationFromImportedFrontier { /* private */ }
// sealed: complete imported-frontier creation reconstitution
impl ReconstitutedSessionCreationFromImportedFrontier {
    // accessors: command(), session(), semantic_entries(), seed_snapshot(),
    //   imported_seed(), applied_result()
}
```

## domain: configuration

```rust
pub struct DirectModelSelection(/* private */);  // identity newtype (see lib.rs shape)
pub struct ModelAlias(/* private */);            // identity newtype (see lib.rs shape)

pub struct FrozenAliasDefinition { /* private */ }
impl FrozenAliasDefinition {
    pub const fn selecting(selected: DirectModelSelection) -> Self;
    // accessors: selected()
}

pub enum ModelSelectionRequest {
    Direct(DirectModelSelection),
    Alias(ModelAlias),
}

pub enum FrozenModelSelection {
    Direct(DirectModelSelection),
    FrozenAlias {
        alias: ModelAlias,
        definition: FrozenAliasDefinition,
    },
}
impl FrozenModelSelection {
    pub const fn selected_direct(self) -> DirectModelSelection;
}

pub enum ModelParameters {
    ProviderDefaults,
}

pub enum KnownProviderFailureRetry {
    Disabled,
}

pub enum ModelFallback {
    Disabled,
}

pub struct EffectiveConfiguration { /* private */ }
impl EffectiveConfiguration {
    pub const fn baseline(model: FrozenModelSelection) -> Self;
    pub const fn with_dangerous_tool_auto_approval(
        model: FrozenModelSelection,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
    ) -> Self;
    pub fn with_model_settings(
        model: FrozenModelSelection,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
        model_settings: ValidatedModelSettings,
    ) -> Option<Self>;
    // accessors: model(), parameters(), known_provider_failure_retry(), model_fallback(),
    // dangerous_tool_auto_approval(), model_settings()
}

pub struct SessionConfigurationDefaultsVersion(/* private u64 */);
impl SessionConfigurationDefaultsVersion {
    pub const fn try_from_u64(value: u64) -> Option<Self>;  // None for zero
    pub const fn as_u64(self) -> u64;
    pub const fn first() -> Self;
    pub const fn checked_next(self) -> Option<Self>;  // None at u64::MAX
}

pub struct SessionSystemPrompt(/* private String */);
impl SessionSystemPrompt {
    pub fn try_new(value: String) -> Result<Self, SessionSystemPromptError>;
    // accessors: as_str(), into_string()
}

pub enum SessionSystemPromptFailure {
    Empty,
    ContainsNull,
}

pub struct SessionSystemPromptError { /* private */ }
impl SessionSystemPromptError {
    // accessors: value(), failure(), into_parts()
}

pub struct SessionConfigurationDefaults { /* private */ }
impl SessionConfigurationDefaults {
    pub const fn new(model: ModelSelectionRequest) -> Self;
    pub const fn with_dangerous_tool_auto_approval(
        model: ModelSelectionRequest,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
    ) -> Self;
    pub const fn complete(
        model: ModelSelectionRequest,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
        system_prompt: Option<SessionSystemPrompt>,
    ) -> Self;
    pub fn complete_with_model_settings(
        model: ModelSelectionRequest,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
        system_prompt: Option<SessionSystemPrompt>,
        model_settings: ValidatedModelSettings,
    ) -> Option<Self>;
    // accessors: model(), dangerous_tool_auto_approval(), system_prompt(), model_settings()
}

pub struct VersionedSessionConfigurationDefaults { /* private */ }
impl VersionedSessionConfigurationDefaults {
    pub const fn establish(defaults: SessionConfigurationDefaults) -> Self;  // version one
    pub fn replace(&self, defaults: SessionConfigurationDefaults) -> Option<Self>;
    pub fn derive_request(
        &self,
        expected: SessionConfigurationDefaultsVersion,
        model: ModelSelectionOverride,
    ) -> Result<VersionCheckedConfigurationRequest, SessionDefaultsVersionMismatch>;
    pub fn derive_request_with_model_settings(
        &self,
        expected: SessionConfigurationDefaultsVersion,
        model: ModelSelectionOverride,
        per_call_model_settings: ModelSettingsOverlay,
    ) -> Result<VersionCheckedConfigurationRequest, SessionDefaultsVersionMismatch>;
    // accessors: version(), defaults()
}
// reconstitution pairing of an arbitrary version with a defaults value is
// crate-private (fail-closed reconstitution, spec/persistence-protocol.md);
// owning reconstitution seams are the producers

pub enum ModelSelectionOverride {
    UseSessionDefault,
    ReplaceWith(ModelSelectionRequest),
}

pub struct ConfigurationRequest { /* private */ }
// sealed: carried inside VersionCheckedConfigurationRequest (derive_request)
impl ConfigurationRequest {
    // accessors: model(), dangerous_tool_auto_approval(), model_settings(), per_call_model_settings()
}

pub struct VersionCheckedConfigurationRequest { /* private */ }
// sealed: VersionedSessionConfigurationDefaults::derive_request
impl VersionCheckedConfigurationRequest {
    // accessors: request(), session_defaults_version()
}

pub struct SessionDefaultsVersionMismatch { /* private */ }
// sealed: Err of derive_request; authoritative rejection, no silent adoption
impl SessionDefaultsVersionMismatch {
    // accessors: expected(), current()
}

pub struct OriginConfiguration { /* private */ }
impl OriginConfiguration {
    pub fn freeze(
        checked: VersionCheckedConfigurationRequest,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<Self, OriginModelSettingsError>;
    pub fn freeze_with_model_settings(
        checked: VersionCheckedConfigurationRequest,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
        capabilities: &ModelCapabilityCatalog,
    ) -> Result<Self, OriginModelSettingsError>;
    pub fn reconstitute_with_model_settings(
        checked: VersionCheckedConfigurationRequest,
        frozen_model: FrozenModelSelection,
        stored_settings: ValidatedModelSettings,
        adjustments: Vec<ModelChangeAdjustment>,
    ) -> Option<Self>;
    // accessors: requested(), session_defaults_version(), effective(),
    // model_settings_adjusted_from(), model_settings_adjustments()
}

pub enum OriginModelSettingsError {
    UnknownAlias(UnknownModelAlias),
    MissingCapabilities { selection: DirectModelSelection },
    Unsupported(UnsupportedModelSetting),
}

pub struct OriginConfigurationReconstitutionInput { /* private */ }
impl OriginConfigurationReconstitutionInput {
    pub const fn new(
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        requested_model: ModelSelectionRequest,
        frozen_model: FrozenModelSelection,
    ) -> Self;
    pub fn reconstitute(self) -> Option<OriginConfiguration>;
}

pub struct UnknownModelAlias { /* private */ }
// sealed: OriginModelSettingsError::UnknownAlias
impl UnknownModelAlias {
    // accessors: alias()
}

pub enum TurnConfigurationProvenance {
    ExplicitOrigin(OriginConfiguration),
    InheritedForReclassifiedSteering(SteeringBinding),
}
```

## domain: model_settings

```rust
pub enum ReasoningLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
}

pub enum FastMode { Disabled, Enabled }

pub enum FastModeOverlay { Inherit, Value(FastMode) }

pub enum AnthropicServiceTier { Auto, StandardOnly }

pub enum OpenAiServiceTier { Auto, Default, Flex, Scale, Priority, Fast }

pub enum CodexCliServiceTier { Default, Priority, Flex }

pub enum ServiceTier {
    Anthropic(AnthropicServiceTier),
    OpenAi(OpenAiServiceTier),
    CodexCli(CodexCliServiceTier),
}

pub enum SettingOverlay<T> {
    Inherit,
    ProviderDefault,
    Value(T),
}

pub struct ModelSettingsOverlay { /* private */ }
impl ModelSettingsOverlay {
    pub const fn inherit_all() -> Self;
    pub const fn new(
        reasoning_level: SettingOverlay<ReasoningLevel>,
        fast_mode: FastModeOverlay,
        service_tier: SettingOverlay<ServiceTier>,
    ) -> Self;
    pub const fn from_effective(settings: EffectiveModelSettings) -> Self;
    // accessors: reasoning_level(), fast_mode(), service_tier()
}

pub struct EffectiveModelSettings { /* private */ }
impl EffectiveModelSettings {
    pub const fn provider_defaults() -> Self;
    pub const fn new(
        reasoning_level: Option<ReasoningLevel>,
        fast_mode: FastMode,
        service_tier: Option<ServiceTier>,
    ) -> Self;
    // accessors: reasoning_level(), fast_mode(), service_tier()
}

pub enum ModelSettingSource { PerCall, Session, Profile, GlobalDefault }

pub struct ResolvedModelSettings { /* private */ }
// sealed: ModelSettingsPrecedence::resolve or ValidatedModelSettings::resolved
impl ResolvedModelSettings {
    // accessors: effective(), reasoning_source(), fast_mode_source(), service_tier_source()
}

pub struct ValidatedModelSettings { /* private */ }
// sealed: provider_defaults, reconstitute, ModelCapabilities::validate_precedence,
// or ModelCapabilities::validate_model_change via AdjustedModelSettings::settings
// and AdjustedModelSettings::into_parts
impl ValidatedModelSettings {
    pub const fn provider_defaults() -> Self;
    pub fn reconstitute(
        precedence: ModelSettingsPrecedence,
        effective: EffectiveModelSettings,
        reasoning_source: Option<ModelSettingSource>,
        fast_mode_source: Option<ModelSettingSource>,
        service_tier_source: Option<ModelSettingSource>,
        validated_for: Option<DirectModelSelection>,
    ) -> Option<Self>;
    // accessors: precedence(), resolved(), effective(), validated_for()
}

pub struct ModelSettingsPrecedence { /* private */ }
impl ModelSettingsPrecedence {
    pub const fn provider_defaults() -> Self;
    pub const fn new(
        per_call: ModelSettingsOverlay,
        session: ModelSettingsOverlay,
        profile: ModelSettingsOverlay,
        global_default: ModelSettingsOverlay,
    ) -> Self;
    pub const fn with_per_call(self, per_call: ModelSettingsOverlay) -> Self;
    pub fn resolve(self) -> ResolvedModelSettings;
    // accessors: per_call(), session(), profile(), global_default()
}

pub enum FastModeSupport {
    Unsupported,
    RequestControl,
    AlternateTarget(ResolvedProviderTarget),
}

pub struct ModelCapabilities { /* private */ }
impl ModelCapabilities {
    pub const fn new(
        reasoning_levels: BTreeSet<ReasoningLevel>,
        fast_mode: FastModeSupport,
        service_tiers: BTreeSet<ServiceTier>,
    ) -> Self;
    pub fn validate_explicit(
        &self,
        selection: DirectModelSelection,
        overlay: ModelSettingsOverlay,
    ) -> Result<(), UnsupportedModelSetting>;
    pub fn validate_precedence(
        &self,
        selection: DirectModelSelection,
        precedence: ModelSettingsPrecedence,
    ) -> Result<ValidatedModelSettings, UnsupportedModelSetting>;
    pub fn adjust_for_model_change(
        &self,
        inherited: EffectiveModelSettings,
    ) -> CompatibleModelSettings;
    pub fn validate_model_change(
        &self,
        selection: DirectModelSelection,
        precedence: ModelSettingsPrecedence,
        caller_overlay: ModelSettingsOverlay,
    ) -> Result<AdjustedModelSettings, UnsupportedModelSetting>;
    pub fn serving_target(
        &self,
        selected: ResolvedProviderTarget,
        fast_mode: FastMode,
    ) -> Option<ResolvedProviderTarget>;
    // accessors: reasoning_levels(), fast_mode(), service_tiers()
}

pub enum UnsupportedModelSetting {
    ReasoningLevel { selection: DirectModelSelection, requested: ReasoningLevel },
    FastMode { selection: DirectModelSelection },
    ServiceTier { selection: DirectModelSelection, requested: ServiceTier },
}

pub enum ModelChangeAdjustment {
    ReasoningLevelClamped { from: ReasoningLevel, to: ReasoningLevel },
    ReasoningLevelCleared { from: ReasoningLevel },
    FastModeDisabled,
    ServiceTierCleared { from: ServiceTier },
}

pub struct CompatibleModelSettings { /* private */ }
// sealed: ModelCapabilities::adjust_for_model_change
impl CompatibleModelSettings {
    pub fn into_parts(self) -> (EffectiveModelSettings, Box<[ModelChangeAdjustment]>);
    // accessors: effective(), adjustments()
}

pub struct AdjustedModelSettings { /* private */ }
// sealed: ModelCapabilities::validate_model_change
impl AdjustedModelSettings {
    pub fn into_parts(self) -> (ValidatedModelSettings, Box<[ModelChangeAdjustment]>);
    // accessors: settings(), adjustments()
}

pub struct SessionModelSettingsChanged { /* private */ }
impl SessionModelSettingsChanged {
    pub fn try_new(
        session: SessionId,
        command_id: DurableCommandId,
        prior_defaults_version: SessionConfigurationDefaultsVersion,
        installed_defaults_version: SessionConfigurationDefaultsVersion,
        prior_model: ModelSelectionRequest,
        installed_model: ModelSelectionRequest,
        prior_settings: ValidatedModelSettings,
        installed_settings: ValidatedModelSettings,
        caller_override: ModelSettingsOverlay,
        adjustments: Vec<ModelChangeAdjustment>,
    ) -> Option<Self>;
    // accessors: session(), command_id(), prior_defaults_version(),
    // installed_defaults_version(), prior_model(), installed_model(), prior_settings(),
    // installed_settings(), caller_override(), adjustments()
}

pub struct TurnModelSettingsResolved { /* private */ }
impl TurnModelSettingsResolved {
    pub fn try_new(
        accepted_input: AcceptedInputId,
        turn: TurnId,
        defaults_version: SessionConfigurationDefaultsVersion,
        selection: FrozenModelSelection,
        per_call_override: ModelSettingsOverlay,
        settings: ValidatedModelSettings,
        adjusted_from_selection: Option<DirectModelSelection>,
        adjustments: Vec<ModelChangeAdjustment>,
    ) -> Option<Self>;
    // accessors: accepted_input(), turn(), defaults_version(), selection(),
    // per_call_override(), settings(), adjusted_from_selection(), adjustments()
}

pub struct ModelCapabilityDefinition { /* private */ }
impl ModelCapabilityDefinition {
    pub const fn new(
        selection: DirectModelSelection,
        capabilities: ModelCapabilities,
    ) -> Self;
    // accessors: selection(), capabilities()
}

pub struct ModelCapabilityCatalog { /* private */ }
impl ModelCapabilityCatalog {
    pub fn try_from_definitions(
        definitions: impl IntoIterator<Item = ModelCapabilityDefinition>,
    ) -> Result<Self, ModelCapabilityCatalogError>;
    pub fn resolve(&self, selection: DirectModelSelection) -> Option<&ModelCapabilities>;
    pub fn iter(&self) -> impl Iterator<Item = (DirectModelSelection, &ModelCapabilities)>;
}

pub enum ModelCapabilityCatalogError {
    DuplicateSelection { selection: DirectModelSelection },
}
```

## domain: accepted_input

```rust
pub struct AcceptedInputLifecycle { /* private */ }
impl AcceptedInputLifecycle {
    pub const fn new(id: AcceptedInputId, disposition: AcceptedInputDisposition) -> Self;
    pub fn consume_as_steering(self, call: ModelCallId)
        -> Result<Self, AcceptedInputLifecycleTransitionError>;
    pub fn reclassify_as_turn_origin(self, turn: TurnId, reason: SteeringReclassificationReason)
        -> Result<Self, AcceptedInputLifecycleTransitionError>;
    pub fn close_not_delivered(self) -> Result<Self, AcceptedInputLifecycleTransitionError>;
    // accessors: id(), disposition()
}

pub enum AcceptedInputLifecycleTransitionError {
    CannotConsumeAsSteering { lifecycle: AcceptedInputLifecycle },
    CannotReclassifyAsTurnOrigin { lifecycle: AcceptedInputLifecycle },
    CannotCloseNotDelivered { lifecycle: AcceptedInputLifecycle },
}
impl AcceptedInputLifecycleTransitionError {
    pub fn into_lifecycle(self) -> AcceptedInputLifecycle;
    // accessors: lifecycle()
}

pub struct SteeringBinding { /* private */ }
impl SteeringBinding {
    pub const fn new(source_turn: TurnId) -> Self;
    // accessors: source_turn()
}

pub enum AcceptedInputDisposition {
    OriginOf(TurnId),
    PendingSteering { binding: SteeringBinding },
    ConsumedAsSteering { call: ModelCallId },
    ReclassifiedAsTurnOrigin { turn: TurnId, reason: SteeringReclassificationReason },
    ClosedNotDelivered,
}
// transitions on a bare disposition are crate-private; AcceptedInputLifecycle
// is the public transition boundary

pub enum SteeringReclassificationReason {
    NoSafePointBeforeTerminal,
}
```

## domain: delivery_request

```rust
pub struct PerInputConfigurationChoices { /* private */ }
impl PerInputConfigurationChoices {
    pub const fn new(
        expected_session_defaults_version: SessionConfigurationDefaultsVersion,
        model: ModelSelectionOverride,
    ) -> Self;
    pub const fn with_model_settings(
        expected_session_defaults_version: SessionConfigurationDefaultsVersion,
        model: ModelSelectionOverride,
        model_settings: ModelSettingsOverlay,
    ) -> Self;
    // accessors: expected_session_defaults_version(), model(), model_settings()
}

pub enum DeliveryRequest {
    StartWhenNoActiveTurn {
        configuration: PerInputConfigurationChoices,
    },
    Interrupt {
        expected_active_turn: TurnId,
        descendant_scope: DescendantTerminationScope,
        configuration: PerInputConfigurationChoices,
    },
    NextSafePoint {
        expected_active_turn: TurnId,
    },
    AfterCurrentTurn {
        expected_active_turn: TurnId,
        configuration: PerInputConfigurationChoices,
    },
}
```

## domain: user_content

```rust
pub struct NonEmptyUnicodeText(/* private String */);
impl NonEmptyUnicodeText {
    pub fn try_new(value: String) -> Result<Self, NonEmptyUnicodeTextError>;
    pub fn into_string(self) -> String;
    // accessors: as_str()
}
// Debug is content-redacted.

pub enum NonEmptyUnicodeTextFailure {
    Empty,
    ContainsNull,
    TooLong,
}

pub struct NonEmptyUnicodeTextError { /* private */ }
impl NonEmptyUnicodeTextError {
    pub fn into_parts(self) -> (String, NonEmptyUnicodeTextFailure);
    // accessors: failure(), value()
}
// Debug is content-redacted.

pub enum AttachmentKind {
    Image,
    Document,
    File,
}

pub struct AttachmentBlobFact { /* private */ }
impl AttachmentBlobFact {
    pub const fn new(digest: BlobDigest, byte_length: NonZeroU64) -> Self;
    // accessors: digest(), byte_length()
}

pub struct DeclaredMediaType(/* private String */);
impl DeclaredMediaType {
    pub const MAX_BYTES: usize;
    pub fn try_new(value: String) -> Result<Self, DeclaredMediaTypeError>;
    // accessor: as_str()
}

pub enum DeclaredMediaTypeFailure {
    Empty,
    TooLong,
    NotVisibleAscii,
}

pub struct DeclaredMediaTypeError { /* private */ }
impl DeclaredMediaTypeError {
    // accessors: failure(), value()
}

pub struct AttachmentDisplayFilename(/* private String */);
impl AttachmentDisplayFilename {
    pub const MAX_BYTES: usize;
    pub fn try_new(value: String) -> Result<Self, AttachmentDisplayFilenameError>;
    // accessor: as_str()
}
// Debug is content-redacted.

pub enum AttachmentDisplayFilenameFailure {
    Empty,
    TooLong,
    ReservedBasename,
    ContainsPathSeparator,
    ContainsNull,
}

pub struct AttachmentDisplayFilenameError { /* private */ }
impl AttachmentDisplayFilenameError {
    // accessors: failure(), value()
}
// Debug is content-redacted.

pub enum UserContentPart {
    Text { value: NonEmptyUnicodeText },
    Attachment {
        digest: BlobDigest,
        kind: AttachmentKind,
        media_type: DeclaredMediaType,
        display_filename: Option<AttachmentDisplayFilename>,
    },
}
impl UserContentPart {
    pub fn try_text(value: String) -> Result<Self, NonEmptyUnicodeTextError>;
}

pub struct UserContent { /* private */ }
impl UserContent {
    pub const MAX_PARTS: usize;
    pub const MAX_TEXT_BYTES: usize;
    pub fn try_text(value: String) -> Result<Self, NonEmptyUnicodeTextError>;
    pub fn try_parts(parts: Vec<UserContentPart>) -> Result<Self, UserContentError>;
    pub fn into_parts(self) -> Vec<UserContentPart>;
    // accessors: parts(), single_text()
}

pub struct UserContentError { /* private */ }
impl UserContentError {
    pub fn into_parts(self) -> (Vec<UserContentPart>, UserContentFailure);
    // accessors: failure(), parts()
}

pub enum UserContentFailure {
    Empty,
    TooManyParts,
    AdjacentTextParts,
    TextTooLarge,
}
```

## domain: submit_input

```rust
pub struct SubmitInput { /* private */ }
impl SubmitInput {
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        delivery: DeliveryRequest,
    ) -> Self;
    pub const fn new_core_interrupt(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        expected_active_turn: TurnId,
        descendant_scope: DescendantTerminationScope,
        configuration: PerInputConfigurationChoices,
    ) -> Self;
    pub fn prepare_session_not_found(self) -> PreparedSubmitInput;
    pub fn prepare_attachment_blob_not_found(
        self,
        digest: BlobDigest,
    ) -> PreparedSubmitInput;
    pub fn prepare_attachment_byte_budget_exceeded(
        self,
        maximum_bytes: u64,
    ) -> PreparedSubmitInput;
    pub fn prepare_when_no_active_turn(
        self,
        session: &Session,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        previous_position: Option<SessionInputPosition>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError>;
    pub fn prepare_when_no_active_turn_with_model_settings(
        self,
        session: &Session,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        previous_position: Option<SessionInputPosition>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
        capabilities: &ModelCapabilityCatalog,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError>;
    pub fn prepare_with_active_turn(
        self,
        scheduling: &AcceptedInputSchedulingProjection,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError>;
    pub fn prepare_with_active_turn_with_model_settings(
        self,
        scheduling: &AcceptedInputSchedulingProjection,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
        capabilities: &ModelCapabilityCatalog,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError>;
    pub fn prepare_with_delegated_active_turn(
        self,
        session: &Session,
        actual_active_turn: TurnId,
        previous_position: Option<SessionInputPosition>,
        existing_interrupt: Option<DurableCommandId>,
        awaiting_approval: bool,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError>;
    // accessors: command_id(), session(), actor(), content(), delivery()
}
// Eq/Hash exclude command_id; all other fields participate

pub enum SubmitInputResult {
    Applied(SubmitInputAppliedResult),
    Rejected(SubmitInputRejectedResult),
}

pub enum SubmitInputAppliedResult {
    TurnOrigin(SubmitInputTurnOriginAppliedResult),
    PendingSteering(SubmitInputPendingSteeringAppliedResult),
}
// sealed: SubmitInput preparation or SubmitInputReconstitutionInput::reconstitute
impl SubmitInputAppliedResult {
    // accessors: accepted_input(), session(), acceptance_position(),
    // disposition(), turn_origin(), pending_steering()
}

pub struct SubmitInputTurnOriginAppliedResult { /* private */ }
// sealed: SubmitInput preparation or checked applied reconstitution
impl SubmitInputTurnOriginAppliedResult {
    pub fn model_settings_event(&self) -> Option<TurnModelSettingsResolved>;
    // accessors: accepted_input(), session(), turn(), disposition(),
    // queue_order(), acceptance_position(), origin_configuration(),
    // applied_interrupt()
}

pub struct SubmitInputPendingSteeringAppliedResult { /* private */ }
// sealed: SubmitInput::prepare_with_active_turn
impl SubmitInputPendingSteeringAppliedResult {
    // accessors: accepted_input(), session(), acceptance_position(), binding()
}

pub enum SubmitInputRejectedResult {
    AttachmentBlobNotFound {
        digest: BlobDigest,
    },
    AttachmentByteBudgetExceeded {
        maximum_bytes: u64,
    },
    SessionNotFound {
        session: SessionId,
    },
    NoActiveTurn {
        session: SessionId,
        expected_active_turn: TurnId,
    },
    ActiveTurnPresent {
        session: SessionId,
        active_turn: TurnId,
    },
    ActiveTurnMismatch {
        session: SessionId,
        expected_active_turn: TurnId,
        actual_active_turn: TurnId,
    },
    SessionDefaultsVersionMismatch {
        session: SessionId,
        expected: SessionConfigurationDefaultsVersion,
        current: SessionConfigurationDefaultsVersion,
    },
    UnknownModelAlias {
        session: SessionId,
        alias: ModelAlias,
    },
    AcceptancePositionExhausted {
        session: SessionId,
        last: SessionInputPosition,
    },
    SafePointUnavailableWhileStopping {
        session: SessionId,
        active_turn: TurnId,
        existing_command: DurableCommandId,
    },
    InterruptAlreadyApplied {
        session: SessionId,
        active_turn: TurnId,
        existing_command: DurableCommandId,
    },
    InterruptUnavailableWhileAwaitingApproval {
        session: SessionId,
        active_turn: TurnId,
    },
}

pub struct PreparedSubmitInput { /* private */ }
// sealed: SubmitInput preparation
impl PreparedSubmitInput {
    pub fn into_parts(self) -> (SubmitInput, SubmitInputResult);
    // accessors: command(), result()
}

pub struct SubmitInputPreparationError { /* private */ }
// sealed: Err of SubmitInput authoritative-state preparation; not terminal
impl SubmitInputPreparationError {
    pub fn into_parts(self) -> (SubmitInput, SubmitInputPreparationFailure);
    // accessors: command(), failure()
}

pub enum SubmitInputPreparationFailure {
    SessionMismatch { provided_session: SessionId },
    TurnCandidateMismatch,
    AcceptedInputCandidateReusesActiveOrigin {
        active_turn: TurnId,
        accepted_input: AcceptedInputId,
    },
    ActiveTurnProjectionMissing,
    InterruptQueueOrderInvalid,
    ModelSettingsResolution(OriginModelSettingsError),
}

pub struct GoalTurnOriginConstructionInput {
    pub generation: GoalGeneration,
    pub source: GoalTurnSource,
    pub session: SessionId,
    pub accepted_input: AcceptedInputId,
    pub turn: TurnId,
    pub acceptance_position: SessionInputPosition,
    pub content: UserContent,
    pub lifecycle: AcceptedInputLifecycle,
    pub queue_accepted_input: AcceptedInputId,
    pub queue_session: SessionId,
    pub queue_turn: TurnId,
    pub queue_order: AcceptedInputQueueOrder,
}

pub struct SubmitInputTerminalSourceReconstitutionInput { /* private */ }
pub struct SubmitInputTerminalSourceConstructionInput {
    /* public named canonical origin, turn, and disposition facts */
}
pub struct SubmitInputInterruptedModelCallReconciliationConstructionInput {
    /* public named canonical origin, turn, ambiguous call, and interrupt facts */
}
pub struct SubmitInputAutomaticReconciliationConstructionInput {
    /* public named canonical origin, turn, ambiguous operation, and recovery-attempt facts */
}
pub struct SubmitInputInterruptedToolReconciliationConstructionInput {
    /* public named canonical origin, turn, ambiguous attempt, and interrupt facts */
}
impl SubmitInputTerminalSourceReconstitutionInput {
    pub fn new(input: SubmitInputTerminalSourceConstructionInput) -> Self;
    pub fn interrupted_model_call_reconciliation(
        input: SubmitInputInterruptedModelCallReconciliationConstructionInput,
    ) -> Self;
    pub fn automatic_reconciliation(
        input: SubmitInputAutomaticReconciliationConstructionInput,
    ) -> Self;
    pub fn interrupted_tool_reconciliation(
        input: SubmitInputInterruptedToolReconciliationConstructionInput,
    ) -> Self;
}

pub struct SubmitInputTurnOriginReconstitutionInput { /* private */ }
pub struct SubmitInputDirectTurnOriginConstructionInput {
    /* public named receipt, lifecycle, and queue-association facts */
}
pub struct SubmitInputReclassifiedTurnOriginConstructionInput {
    /* public named receipt, lifecycle, queue-association, and terminal-source facts */
}
impl SubmitInputTurnOriginReconstitutionInput {
    pub fn from_goal(input: GoalTurnOriginConstructionInput) -> Self;
    pub fn new(input: SubmitInputDirectTurnOriginConstructionInput) -> Self;
    pub fn reclassified(input: SubmitInputReclassifiedTurnOriginConstructionInput) -> Self;
}

pub struct NonAcceptedTurnPredecessorReconstitutionInput {
    pub session: SessionId,
    pub turn: TurnId,
}
pub struct SubmitInputAppliedTurnOriginReconstitutionInput {
    /* public named command, result, accepted-input, accepted/non-accepted predecessor, queue, and configuration facts */
}
pub struct SubmitInputAppliedPendingSteeringReconstitutionInput {
    /* public named command, result, source-turn, and accepted-input facts */
}
pub struct SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput {
    /* public named command, actor, session, and unavailable-digest facts */
}
pub struct SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput {
    /* public named command, actor, session, and maximum-byte facts */
}
pub struct SubmitInputRejectedSessionNotFoundReconstitutionInput {
    /* public named command, actor, and absent-session facts */
}
pub struct SubmitInputRejectedNoActiveTurnReconstitutionInput {
    /* public named command, actor, session, and expected-turn facts */
}
pub struct SubmitInputRejectedActiveTurnPresentReconstitutionInput {
    /* public named command, result, and canonical active-turn-origin facts */
}
pub struct SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
    /* public named command, expected/actual turn, and canonical origin facts */
}
pub struct SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
    /* public named command, defaults-version, and optional active-origin facts */
}
pub struct SubmitInputRejectedUnknownModelAliasReconstitutionInput {
    /* public named command, alias, defaults, and optional active-origin facts */
}
pub struct SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
    /* public named command, final-position, and optional active-origin facts */
}
pub struct SubmitInputRejectedSafePointUnavailableWhileStoppingReconstitutionInput {
    /* public named command, active-origin, and existing-interrupt facts */
}
pub struct SubmitInputRejectedInterruptAlreadyAppliedReconstitutionInput {
    /* public named command, active-origin, and existing-interrupt facts */
}
pub struct SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput {
    /* public named command, active-turn, and canonical origin facts */
}
pub struct SubmitInputReconstitutionInput { /* private */ }
impl SubmitInputReconstitutionInput {
    pub fn applied_turn_origin(
        input: SubmitInputAppliedTurnOriginReconstitutionInput,
    ) -> Self;
    pub fn applied_pending_steering(
        input: SubmitInputAppliedPendingSteeringReconstitutionInput,
    ) -> Self;
    pub fn rejected_attachment_blob_not_found(
        input: SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput,
    ) -> Self;
    pub fn rejected_attachment_byte_budget_exceeded(
        input: SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput,
    ) -> Self;
    pub fn rejected_safe_point_unavailable_while_stopping(
        input: SubmitInputRejectedSafePointUnavailableWhileStoppingReconstitutionInput,
    ) -> Self;
    pub fn rejected_interrupt_already_applied(
        input: SubmitInputRejectedInterruptAlreadyAppliedReconstitutionInput,
    ) -> Self;
    pub fn rejected_interrupt_unavailable_while_awaiting_approval(
        input: SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput,
    ) -> Self;
    pub fn rejected_session_not_found(
        input: SubmitInputRejectedSessionNotFoundReconstitutionInput,
    ) -> Self;
    pub fn rejected_no_active_turn(
        input: SubmitInputRejectedNoActiveTurnReconstitutionInput,
    ) -> Self;
    pub fn rejected_active_turn_present(
        input: SubmitInputRejectedActiveTurnPresentReconstitutionInput,
    ) -> Self;
    pub fn rejected_active_turn_mismatch(
        input: SubmitInputRejectedActiveTurnMismatchReconstitutionInput,
    ) -> Self;
    pub fn rejected_defaults_version_mismatch(
        input: SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput,
    ) -> Self;
    pub fn rejected_unknown_model_alias(
        input: SubmitInputRejectedUnknownModelAliasReconstitutionInput,
    ) -> Self;
    pub fn rejected_acceptance_position_exhausted(
        input: SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput,
    ) -> Self;
    pub fn reconstitute(self)
        -> Result<ReconstitutedSubmitInput, SubmitInputReconstitutionError>;
    // accessors: command()
}

pub enum SubmitInputReconstitutionFailure {
    StoredActorMismatch,
    AppliedDeliveryIsNotTurnOrigin,
    AppliedDeliveryIsNotNextSafePoint,
    ResultSessionMismatch,
    AttachmentDigestMismatch,
    AttachmentBudgetMismatch,
    AcceptedCommandMismatch,
    AcceptedInputMismatch,
    AcceptedSessionMismatch,
    AcceptedContentMismatch,
    AcceptedDeliveryMismatch,
    AcceptedDispositionMismatch,
    SteeringSourceTurnMismatch,
    SteeringSourceTurnOriginMismatch,
    SteeringSourceAcceptedInputReused,
    SteeringSourceCommandReused,
    SteeringAcceptanceDoesNotFollowSourceOrigin,
    QueueSessionMismatch,
    QueueTurnMismatch,
    AfterCurrentPredecessorOriginMismatch,
    AfterCurrentPredecessorAcceptedInputReused,
    AfterCurrentPredecessorCommandReused,
    AfterCurrentAcceptanceDoesNotFollowPredecessorOrigin,
    QueuePositionMismatch,
    QueuePriorityMismatch,
    ActiveTurnPresentRejectionMismatch,
    ExpectedActiveTurnMismatch,
    RejectedActiveTurnsAreEqual,
    RejectionActiveTurnOriginMismatch,
    RejectionActiveTurnOriginCommandReused,
    RejectionHasNoExplicitOriginConfiguration,
    ExpectedDefaultsVersionMismatch,
    RejectedDefaultsVersionsAreEqual,
    DefaultsSessionMismatch,
    DefaultsVersionMismatch,
    RequestedModelMismatch,
    FrozenModelMismatch,
    UnknownAliasMismatch,
    RejectionDidNotSelectAlias,
    PositionIsNotExhausted,
    StoppingRejectionMismatch,
    ExistingInterruptMismatch,
}

pub struct SubmitInputReconstitutionError { /* private */ }
// sealed: Err of SubmitInputReconstitutionInput::reconstitute
impl SubmitInputReconstitutionError {
    pub fn into_parts(
        self,
    ) -> (SubmitInputReconstitutionInput, SubmitInputReconstitutionFailure);
    // accessors: failure(), input()
}

pub struct ReconstitutedSubmitInput { /* private */ }
// sealed: SubmitInputReconstitutionInput::reconstitute; authorizes no effect
impl ReconstitutedSubmitInput {
    pub fn into_parts(self) -> (SubmitInput, SubmitInputResult);
    // accessors: command(), result()
}
```

## domain: queue_order

```rust
pub struct SessionInputPosition(/* private u64 */);
impl SessionInputPosition {
    pub const fn try_from_u64(value: u64) -> Option<Self>;  // None for zero
    pub const fn as_u64(self) -> u64;
    pub const fn first() -> Self;
    pub const fn checked_next(self) -> Option<Self>;  // None at u64::MAX
}

pub enum AcceptedInputQueuePriority {
    Ordinary,
    InterruptImmediatelyAfter { predecessor: TurnId },
}

pub struct AcceptedInputQueueOrder { /* private */ }
impl AcceptedInputQueueOrder {
    pub const fn ordinary(acceptance_position: SessionInputPosition) -> Self;
    pub const fn interrupt_immediately_after(
        acceptance_position: SessionInputPosition,
        predecessor: TurnId,
    ) -> Self;
    // accessors: acceptance_position(), priority()
}
// no form can carry a direct starting predecessor (INV-009)

pub struct AcceptedInputQueueWork { /* private */ }
impl AcceptedInputQueueWork {
    pub const fn new(session: SessionId, turn: TurnId, order: AcceptedInputQueueOrder) -> Self;
    // accessors: session(), turn(), order()
}

pub enum AcceptedInputQueueOrderError {
    MixedSessions {
        first_session: SessionId,
        second_session: SessionId,
    },
    DuplicateTurn {
        turn: TurnId,
    },
    DuplicateAcceptancePosition {
        position: SessionInputPosition,
        first_turn: TurnId,
        second_turn: TurnId,
    },
    MissingInterruptPredecessor {
        turn: TurnId,
        predecessor: TurnId,
    },
    SelfInterruptPredecessor {
        turn: TurnId,
    },
    MultipleInterruptSuccessors {
        predecessor: TurnId,
        first_successor: TurnId,
        second_successor: TurnId,
    },
    InterruptCycle {
        turn: TurnId,
    },
    InterruptPositionNotAfterPredecessor {
        turn: TurnId,
        predecessor: TurnId,
        position: SessionInputPosition,
        predecessor_position: SessionInputPosition,
    },
    InterruptPredecessorChronologyReversed {
        earlier_interrupt: TurnId,
        earlier_predecessor: TurnId,
        later_interrupt: TurnId,
        later_predecessor: TurnId,
    },
}

pub fn derive_accepted_input_total_order(
    currently_known_work: impl IntoIterator<Item = AcceptedInputQueueWork>,
) -> Result<Vec<TurnId>, AcceptedInputQueueOrderError>;
```

## domain: turn_lifecycle

```rust
pub enum AcceptedInputStartingLineage {
    FirstInSession,
    After { immediate_predecessor: TurnId },
}

pub struct AcceptedInputTurnStart { /* private */ }
// sealed: checked scheduling reconstitution and live eligibility are the only
// producers
impl AcceptedInputTurnStart {
    // accessors: lineage(), frontier()
}

pub enum IssuedOperationRef {
    ModelCall(ModelCallId),
    ToolAttempt(ToolAttemptId),
}

pub struct NonEmptyIssuedOperationRefs { /* private */ }
impl NonEmptyIssuedOperationRefs {
    pub fn try_from_operations(
        operations: impl IntoIterator<Item = IssuedOperationRef>,
    ) -> Result<Self, NonEmptyIssuedOperationRefsError>;
    pub fn operation_count(&self) -> usize;
    pub fn contains(&self, operation: IssuedOperationRef) -> bool;
    pub fn iter(&self) -> impl ExactSizeIterator<Item = IssuedOperationRef> + '_;
}
// canonical set; empty and duplicate input rejected

pub enum NonEmptyIssuedOperationRefsError {
    Empty,
    Duplicate { operation: IssuedOperationRef },
}

pub struct AppliedStopForReconciliationProof { /* private */ }
// sealed: no public producer yet; a later exact-set command-result slice
// supplies the trusted producer
impl AppliedStopForReconciliationProof {
    // accessors: decision_command(), turn()
}

pub enum ReconciliationReason {
    UserChoseReconciliation { decision: AppliedStopForReconciliationProof },
    InterruptRequiresReconciliation { interrupt: AppliedInterruptProof },
    FatalMismatchRequiresReconciliation { causes: FatalMismatchStopCauses },
    AutomaticRecovery { attempt: NonZeroU32 },
}

pub struct ReconciliationMarker { /* private */ }
// sealed: crate-private construction from the fatal-mismatch candidate binding;
// no public producer
impl ReconciliationMarker {
    // accessors: ambiguous_operations(), reason()
}

pub enum ActiveTurnPhase {
    Running { current_attempt: CurrentTurnAttempt },
    AwaitingApproval { request: ToolRequestId },
    AwaitingChild { wait: ChildWait },
    AwaitingRecoveryDecision {
        ambiguous_operations: NonEmptyIssuedOperationRefs,
        applied_interrupt: Option<AppliedInterruptProof>,
    },
    AwaitingRunnerRecovery {
        runner: RunnerId,
        placement_revision: RunnerGeneration,
        optional_tool_attempt: Option<ToolAttemptId>,
    },
}
impl ActiveTurnPhase {
    pub const fn retains_progressing_slot(&self) -> bool;  // always true
}

pub enum TurnDisposition {
    Completed,
    Refused,
    Failed,
    Cancelled { cause: AppliedInterruptProof },
    ReconciliationRequired { marker: ReconciliationMarker },
    Retired,
}

pub enum TurnTerminalCause {
    Completed,
    ModelRefusal,
    InterruptApplied,
    ModelCallAmbiguous,
    ToolAttemptAmbiguous,
    ModelCallFailed,
    ModelTargetUnavailable,
    AttachmentPreparationFailed,
    CapabilityPreparationFailed,
    ToolRoundLimitReached,
    ToolAttemptLost,
    CredentialPoolExhausted,
    HeadlessApprovalEscalation,
    AbandonedAtRestart,
    WatchdogStaleTurn,
    ContextHeadroomExhausted,
    ContextCompactionWall,
    ContextCompactionFailed,
    ReportedUsageContextCompactionExhausted,
    ReportedUsageContextStillExceeded,
    UnclassifiedFailure,
    GoalTurnIneligible,
}
```

## domain: turn_eligibility

```rust
pub enum AcceptedInputTurnSchedulingRecordState {
    Queued,
    Active {
        starting_lineage: AcceptedInputStartingLineage,
        starting_frontier: ContextFrontierId,
        phase: ActiveTurnSchedulingReconstitutionInput,
    },
    TerminalFailed {
        starting_lineage: AcceptedInputStartingLineage,
        starting_frontier: ContextFrontierId,
        terminal_execution: Option<FailedTurnExecutionReconstitutionInput>,
        terminal_frontier: ContextFrontierId,
    },
    TerminalCompleted {
        starting_lineage: AcceptedInputStartingLineage,
        starting_frontier: ContextFrontierId,
        completing_attempt: TurnAttemptId,
        completing_attempt_end: TerminalAttemptEndReconstitutionInput,
        completing_call: ModelCallId,
        terminal_frontier: ContextFrontierId,
    },
    TerminalRefused {
        starting_lineage: AcceptedInputStartingLineage,
        starting_frontier: ContextFrontierId,
        refusing_attempt: TurnAttemptId,
        refusing_attempt_end: TerminalAttemptEndReconstitutionInput,
        refusing_call: ModelCallId,
        terminal_frontier: ContextFrontierId,
    },
    TerminalCancelled {
        starting_lineage: AcceptedInputStartingLineage,
        starting_frontier: ContextFrontierId,
        terminal_execution: CancelledTurnExecutionReconstitutionInput,
        terminal_frontier: ContextFrontierId,
    },
    TerminalReconciliationRequired {
        starting_lineage: AcceptedInputStartingLineage,
        starting_frontier: ContextFrontierId,
        reconciling_attempt: TurnAttemptId,
        reconciling_attempt_end: TerminalAttemptEndReconstitutionInput,
        ambiguous_call: ModelCallId,
        authority: AutomaticReconciliationAuthority,
        terminal_frontier: ContextFrontierId,
    },
    TerminalToolReconciliationRequired {
        starting_lineage: AcceptedInputStartingLineage,
        starting_frontier: ContextFrontierId,
        reconciling_attempt: TurnAttemptId,
        reconciling_attempt_end: TerminalAttemptEndReconstitutionInput,
        tool_batch: ToolBatch,
        authority: AutomaticReconciliationAuthority,
        terminal_frontier: ContextFrontierId,
    },
}

pub enum AutomaticReconciliationAuthority {
    AppliedInterrupt(AppliedInterruptCommandResult),
    AutomaticRecovery { attempt: NonZeroU32 },
}

pub struct FailedTurnExecutionReconstitutionInput { /* private */ }
impl FailedTurnExecutionReconstitutionInput {
    pub const fn attempt_only(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        attempt_disposition: UnstoppedAttemptDisposition,
    ) -> Self;
    pub const fn with_call(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        attempt_disposition: UnstoppedAttemptDisposition,
        ended_call: ModelCallId,
    ) -> Self;
    pub const fn attempt_only_after_cancellation(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        disposition: CancellationStopDisposition,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self;
    pub const fn with_call_after_cancellation(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        disposition: CancellationStopDisposition,
        interrupt: AppliedInterruptCommandResult,
        ended_call: ModelCallId,
    ) -> Self;
    pub fn with_terminal_tool_attempts(
        self,
        terminal_tool_attempts: Vec<EndedToolAttempt>,
    ) -> Self;
    pub fn with_terminal_tool_denials(
        self,
        terminal_tool_denials: Vec<ToolApprovalResolution>,
    ) -> Self;
    // accessors: owning_turn(), ended_attempt(), attempt_end(), ended_call(),
    // terminal_tool_attempts(), terminal_tool_denials()
}

pub struct TerminalAttemptEndReconstitutionInput { /* private */ }
impl TerminalAttemptEndReconstitutionInput {
    pub const fn without_stop(disposition: UnstoppedAttemptDisposition) -> Self;
    pub const fn after_cancellation(
        disposition: CancellationStopDisposition,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self;
    pub const fn yielded_to_runner_recovery(
        interrupt: AppliedInterruptCommandResult,
    ) -> Self;
    // accessors: end(), interrupt()
}

pub struct CancelledTurnExecutionReconstitutionInput { /* private */ }
impl CancelledTurnExecutionReconstitutionInput {
    pub const fn new(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        attempt_end: TerminalAttemptEndReconstitutionInput,
        ended_call: Option<ModelCallId>,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self;
    pub fn with_terminal_tool_attempts(
        self,
        terminal_tool_attempts: Vec<EndedToolAttempt>,
    ) -> Self;
    pub fn with_terminal_tool_denials(
        self,
        terminal_tool_denials: Vec<ToolApprovalResolution>,
    ) -> Self;
    // accessors: terminal_tool_attempts(), terminal_tool_denials()
}

pub struct ActiveTurnSchedulingReconstitutionInput { /* private */ }
impl ActiveTurnSchedulingReconstitutionInput {
    pub const fn prepared(
        owning_turn: TurnId,
        current_attempt: TurnAttemptId,
    ) -> Self;
    pub const fn running(
        owning_turn: TurnId,
        current_attempt: TurnAttemptId,
    ) -> Self;
    pub fn with_executing_tool_batch(self, batch: &ToolBatch) -> Self;
    pub const fn stop_requested(
        owning_turn: TurnId,
        current_attempt: TurnAttemptId,
        call: ModelCallId,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self;
    pub fn awaiting_approval(
        owning_turn: TurnId,
        batch: &ToolBatch,
    ) -> Option<Self>;
    pub fn awaiting_child(
        owning_turn: TurnId,
        batch: &ToolBatch,
    ) -> Option<Self>;
    pub const fn awaiting_tool_recovery(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        wait: AwaitingToolRecovery,
    ) -> Self;
    pub const fn awaiting_tool_recovery_after_restart(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        wait: AwaitingToolRecovery,
    ) -> Self;
    pub const fn awaiting_tool_recovery_after_cancellation(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        wait: AwaitingToolRecovery,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self;
    pub const fn awaiting_tool_recovery_after_cancellation_restart(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        wait: AwaitingToolRecovery,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self;
    pub const fn awaiting_model_call_recovery(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        ambiguous_call: ModelCallId,
    ) -> Self;
    pub const fn awaiting_model_call_recovery_after_restart(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        ambiguous_call: ModelCallId,
    ) -> Self;
    pub const fn awaiting_model_call_recovery_after_cancellation(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        ambiguous_call: ModelCallId,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self;
    pub const fn awaiting_model_call_recovery_after_cancellation_restart(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        ambiguous_call: ModelCallId,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self;
    pub const fn awaiting_runner_recovery(
        owning_turn: TurnId,
        runner: RunnerId,
        placement_revision: RunnerGeneration,
        interrupted_tool_attempt: Option<ToolAttemptId>,
        source_frontier: Option<ContextFrontierId>,
    ) -> Self;
    // accessor: owning_turn()
}

pub struct SessionAcceptanceTailEntryReconstitutionInput { /* private */ }
impl SessionAcceptanceTailEntryReconstitutionInput {
    pub const fn new(
        session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        position: SessionInputPosition,
        delivery: DeliveryRequest,
    ) -> Self;
    pub const fn retired_goal_origin(
        session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        position: SessionInputPosition,
        delivery: DeliveryRequest,
    ) -> Self;
    // accessors: session(), accepted_input(), position(), delivery()
}

pub struct SessionAcceptanceTailReconstitutionInput { /* private */ }
impl SessionAcceptanceTailReconstitutionInput {
    pub fn new(
        session: SessionId,
        anchor: AcceptedInputId,
        observed_last_position: SessionInputPosition,
        entries: Vec<SessionAcceptanceTailEntryReconstitutionInput>,
    ) -> Self;
    // accessors: session(), anchor(), observed_last_position(), entries()
}

pub struct ConsumedSteeringReconstitutionInput { /* private */ }
impl ConsumedSteeringReconstitutionInput {
    pub const fn new(
        session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        acceptance_position: SessionInputPosition,
        source_turn: TurnId,
    ) -> Self;
    // accessors: session(), accepted_input(), acceptance_position(), source_turn()
}

pub struct SteeringContinuationRoundReconstitutionInput { /* private */ }
impl SteeringContinuationRoundReconstitutionInput {
    pub const fn new(
        call: ModelCallId,
        round_tool_attempts: Vec<EndedToolAttempt>,
        round_tool_denials: Vec<ToolApprovalResolution>,
    ) -> Self;
    // accessors: call(), round_tool_attempts(), round_tool_denials()
}

pub struct ContinuationRoundReconstitutionInput { /* private */ }
impl ContinuationRoundReconstitutionInput {
    pub const fn new(
        call: ModelCallId,
        round_tool_attempts: Vec<EndedToolAttempt>,
        round_tool_denials: Vec<ToolApprovalResolution>,
    ) -> Self;
    // accessors: call(), round_tool_attempts(), round_tool_denials()
}

pub struct PendingSteeringInput { /* private */ }
impl PendingSteeringInput {
    pub fn reconstitute(
        accepted_input: AcceptedInputLifecycle,
        acceptance_position: SessionInputPosition,
        source_turn: TurnId,
    ) -> Option<Self>;
    // accessors: accepted_input(), lifecycle(), acceptance_position()
}

pub struct ConsumedSteeringInput { /* private */ }
// sealed: checked AcceptedInputSchedulingProjection::active_turn_execution
impl ConsumedSteeringInput {
    // accessors: accepted_input(), lifecycle(), acceptance_position(), source_turn()
}

pub struct AcceptedInputTurnSchedulingRecord { /* private */ }
impl AcceptedInputTurnSchedulingRecord {
    pub fn new(
        stored_session: SessionId,
        turn: TurnId,
        accepted_input_session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        queue_session: SessionId,
        queue_turn: TurnId,
        order: AcceptedInputQueueOrder,
        origin_delivery: DeliveryRequest,
        origin_configuration: OriginConfiguration,
        state: AcceptedInputTurnSchedulingRecordState,
    ) -> Self;
    pub fn reclassified(
        stored_session: SessionId,
        turn: TurnId,
        accepted_input_session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        queue_session: SessionId,
        queue_turn: TurnId,
        order: AcceptedInputQueueOrder,
        origin_delivery: DeliveryRequest,
        binding: SteeringBinding,
        source_configuration: OriginConfiguration,
        state: AcceptedInputTurnSchedulingRecordState,
    ) -> Self;
    pub fn without_legacy_model_identity_boundary(self) -> Self;
    // accessors: stored_session(), turn(), accepted_input_session(),
    // accepted_input(), queue_session(), queue_turn(), order(),
    // origin_delivery(), origin_configuration(), configuration_provenance(), state()
}

pub enum DelegatedTurnSchedulingState {
    Active,
    RuntimeTerminal,
    TerminalCompleted,
    TerminalRefused,
    TerminalFailed,
    TerminalCancelled,
    TerminalReconciliationRequired,
}

pub struct DelegatedTurnSchedulingFact { /* private */ }
impl DelegatedTurnSchedulingFact {
    pub const fn new(
        turn: TurnId,
        defaults_version: SessionConfigurationDefaultsVersion,
        selected: DirectModelSelection,
        state: DelegatedTurnSchedulingState,
    ) -> Self;
    // accessors: turn(), defaults_version(), selected(), state()
}

pub struct AcceptedInputSchedulingReconstitutionInput { /* private */ }
impl AcceptedInputSchedulingReconstitutionInput {
    pub fn new(
        session: Session,
        turns: Vec<AcceptedInputTurnSchedulingRecord>,
        semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
        snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
        active_acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
    ) -> Self;
    pub fn reconstitute(self)
        -> Result<
            AcceptedInputSchedulingProjection,
            AcceptedInputSchedulingReconstitutionError,
        >;
    pub fn with_model_call_facts(
        self,
        pinned_targets: Vec<PinnedProviderTargetReconstitutionInput>,
        model_calls: Vec<ModelCallReconstitutionInput>,
    ) -> Self;
    pub fn with_context_compaction_facts(
        self,
        calls: Vec<ContextCompactionModelCallReconstitutionInput>,
        compactions: Vec<ContextCompactionReconstitutionInput>,
    ) -> Self;
    pub fn with_consumed_steering_facts(
        self,
        consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    ) -> Self;
    pub fn with_delegated_consumed_steering_facts(
        self,
        consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    ) -> Self;
    pub fn with_delegated_turn_facts(
        self,
        delegated_turns: Vec<DelegatedTurnSchedulingFact>,
    ) -> Self;
    pub fn with_steering_continuation_rounds(
        self,
        steering_continuation_rounds: Vec<SteeringContinuationRoundReconstitutionInput>,
    ) -> Self;
    pub fn with_continuation_rounds(
        self,
        continuation_rounds: Vec<ContinuationRoundReconstitutionInput>,
    ) -> Self;
    pub fn with_imported_session(
        self,
        imported_session: ReconstitutedImportedSession,
    ) -> Self;
    pub fn with_preceding_non_accepted_terminal(
        self,
        session: SessionId,
        predecessor: TurnId,
        successor: TurnId,
        terminal_frontier: ContextFrontierId,
        selected: DirectModelSelection,
    ) -> Self;
    // accessors: session(), imported_session(), turns(), semantic_entries(),
    // snapshots(), pinned_targets(), model_calls(), consumed_steering(),
    // delegated_consumed_steering(), delegated_turns(),
    // steering_continuation_rounds(), continuation_rounds(),
    // active_acceptance_tail()
}

pub enum AcceptedInputSchedulingReconstitutionFailure {
    UnsupportedSessionAncestry,
    MissingImportedSession,
    UnexpectedImportedSession,
    ImportedSessionMismatch,
    UnsupportedSemanticEntry { entry: SemanticTranscriptEntryId },
    TurnSessionMismatch { turn: TurnId },
    AcceptedInputSessionMismatch { turn: TurnId },
    QueueSessionMismatch { turn: TurnId },
    QueueTurnMismatch { turn: TurnId },
    AcceptedInputOriginMismatch { turn: TurnId },
    OriginDeliveryMismatch { turn: TurnId },
    DuplicateAcceptedInput { accepted_input: AcceptedInputId },
    InvalidQueueOrder { error: AcceptedInputQueueOrderError },
    SemanticEntrySourceSessionMismatch { entry: SemanticTranscriptEntryId },
    DuplicateSemanticEntry { entry: SemanticTranscriptEntryRef },
    SemanticEntrySubjectMissing { entry: SemanticTranscriptEntryId },
    SemanticEntryStateMismatch { entry: SemanticTranscriptEntryId },
    DuplicateSemanticEntryForSubject { entry: SemanticTranscriptEntryId },
    DelegatedTurnFactMismatch { turn: TurnId },
    ConsumedSteeringSessionMismatch { accepted_input: AcceptedInputId },
    DuplicateConsumedSteering { accepted_input: AcceptedInputId },
    SteeringSemanticEntryMismatch { entry: SemanticTranscriptEntryId },
    ConsumedSteeringMismatch { accepted_input: AcceptedInputId },
    SteeringContinuationRoundMismatch { call: ModelCallId },
    ContinuationRoundMismatch { call: ModelCallId },
    SemanticEntryCallMissing {
        entry: SemanticTranscriptEntryId,
        call: ModelCallId,
    },
    SemanticEntryCallMismatch {
        entry: SemanticTranscriptEntryId,
        call: ModelCallId,
    },
    DuplicateModelCall { call: ModelCallId },
    DuplicateModelCallIdentityAcrossKinds { call: ModelCallId },
    DuplicatePinnedTarget { turn: TurnId },
    PinnedTargetMissing { call: ModelCallId },
    UnreferencedPinnedTarget { turn: TurnId },
    ModelCallSnapshotMissing { call: ModelCallId },
    InvalidModelCall { call: ModelCallId },
    CompactionCallSnapshotMissing { call: ModelCallId },
    DuplicateCompactionCall { call: ModelCallId },
    InvalidCompactionCall { call: ModelCallId },
    CompactionSnapshotMissing { compaction: ContextCompactionId },
    CompactionEvidenceMissing { compaction: ContextCompactionId },
    InvalidCompaction { compaction: ContextCompactionId },
    DuplicateCompaction { compaction: ContextCompactionId },
    UnreferencedCompactionEvidence { call: ModelCallId },
    InvalidCompactionChain { compaction: ContextCompactionId },
    UnreferencedModelCall { call: ModelCallId },
    TerminalModelCallMissing { turn: TurnId, call: ModelCallId },
    TerminalModelCallMismatch { turn: TurnId },
    RecoveryModelCallMissing { turn: TurnId, call: ModelCallId },
    RecoveryModelCallMismatch { turn: TurnId },
    MissingOriginEntry { turn: TurnId },
    MissingFailureEntry { turn: TurnId },
    MissingCompletionEntry { turn: TurnId },
    MissingCancellationEntry { turn: TurnId },
    CurrentAttemptOwnershipMismatch { turn: TurnId, attempt: TurnAttemptId },
    TerminalAttemptOwnershipMismatch { turn: TurnId, attempt: TurnAttemptId },
    TerminalAttemptEndMismatch { turn: TurnId, attempt: TurnAttemptId },
    DuplicateCurrentAttempt { attempt: TurnAttemptId },
    ActivePhaseEvidenceMismatch {
        turn: TurnId,
        accepted_input: AcceptedInputId,
    },
    MissingActiveAcceptanceTail { turn: TurnId },
    UnexpectedActiveAcceptanceTail,
    AcceptanceTailSessionMismatch {
        expected: SessionId,
        actual: SessionId,
    },
    AcceptanceTailAnchorMismatch {
        turn: TurnId,
        expected: AcceptedInputId,
        actual: AcceptedInputId,
    },
    AcceptanceTailEntrySessionMismatch { accepted_input: AcceptedInputId },
    DuplicateAcceptanceTailEntry { accepted_input: AcceptedInputId },
    AcceptanceTailPositionMismatch {
        accepted_input: AcceptedInputId,
        expected: SessionInputPosition,
        actual: SessionInputPosition,
    },
    AcceptanceTailLastPositionMismatch {
        expected: SessionInputPosition,
        actual: Option<SessionInputPosition>,
    },
    AcceptanceTailDispositionMismatch { accepted_input: AcceptedInputId },
    SnapshotOwningSessionMismatch { snapshot: ContextFrontierId },
    DuplicateSnapshot { snapshot: ContextFrontierId },
    InvalidSnapshotMembership { snapshot: ContextFrontierId },
    SnapshotEntryMissing {
        snapshot: ContextFrontierId,
        entry: SemanticTranscriptEntryRef,
    },
    StartingSnapshotMissing { turn: TurnId },
    TerminalSnapshotMissing { turn: TurnId },
    InvalidLifecycleOrder { turn: TurnId },
    StartingLineageMismatch {
        turn: TurnId,
        expected: AcceptedInputStartingLineage,
        actual: AcceptedInputStartingLineage,
    },
    StartingFrontierMismatch { turn: TurnId },
    TerminalFrontierMismatch { turn: TurnId },
    UnreferencedSnapshot { snapshot: ContextFrontierId },
}

pub struct AcceptedInputSchedulingReconstitutionError { /* private */ }
// sealed: Err of AcceptedInputSchedulingReconstitutionInput::reconstitute
impl AcceptedInputSchedulingReconstitutionError {
    pub fn into_parts(
        self,
    ) -> (
        AcceptedInputSchedulingReconstitutionInput,
        AcceptedInputSchedulingReconstitutionFailure,
    );
    // accessors: input(), failure()
}

pub enum AcceptedInputTurnSchedulingStatus {
    Queued,
    Active,
    TerminalFailed,
    TerminalCompleted,
    TerminalRefused,
    TerminalCancelled,
    TerminalReconciliationRequired,
}

pub struct AcceptedInputTurnSchedulingProjection { /* private */ }
// sealed: AcceptedInputSchedulingReconstitutionInput::reconstitute
impl AcceptedInputTurnSchedulingProjection {
    // accessors: session(), turn(), accepted_input(), order(),
    // origin_configuration(), configuration_provenance(), status(), start(), active_phase(),
    // failed_terminal_frontier(), terminal_frontier()
}

pub struct AcceptedInputSchedulingProjection { /* private */ }
// sealed: AcceptedInputSchedulingReconstitutionInput::reconstitute
impl AcceptedInputSchedulingProjection {
    pub fn turns(
        &self,
    ) -> impl ExactSizeIterator<Item = &AcceptedInputTurnSchedulingProjection>;
    pub fn turn(
        &self,
        turn: TurnId,
    ) -> Option<&AcceptedInputTurnSchedulingProjection>;
    pub fn active_turn(&self) -> Option<&AcceptedInputTurnSchedulingProjection>;
    pub fn active_turn_execution(&self) -> Option<ActivatedAcceptedInputTurn>;
    pub fn active_rendered_frontier_origins(
        &self,
    ) -> Option<Vec<AcceptedInputId>>;
    pub fn apply_interrupt_to_model_call_recovery(
        self,
        interrupt: AppliedInterruptCommandResult,
        identities: AmbiguousModelCallTurnIdentities,
    ) -> Result<ReconciliationRequiredModelCallTurn, ModelCallClosureError>;
    pub fn apply_automatic_reconciliation(
        self,
        attempt: NonZeroU32,
        identities: AmbiguousModelCallTurnIdentities,
    ) -> Result<ReconciliationRequiredModelCallTurn, ModelCallClosureError>;
    pub fn apply_automatic_tool_reconciliation(
        self,
        wait: AwaitingToolRecovery,
        tool_attempt: EndedToolAttempt,
        result_projection: PreparedToolResultProjection,
        recovery_attempt: NonZeroU32,
        identities: AmbiguousModelCallTurnIdentities,
    ) -> Result<ReconciliationRequiredToolTurn, ModelCallClosureError>;
    pub fn apply_interrupt_to_runner_recovery(
        self,
        source_snapshot: ResolvedContextFrontierSnapshot,
        result_projection: Option<PreparedToolResultProjection>,
        interrupt: AppliedInterruptCommandResult,
        identities: CancelledModelCallTurnIdentities,
    ) -> Result<CancelledModelCallTurn, ModelCallClosureError>;
    pub fn apply_interrupt_to_runner_tool_recovery(
        self,
        wait: AwaitingToolRecovery,
        tool_attempt: EndedToolAttempt,
        yielded_attempt: TurnAttemptId,
        result_projection: PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: AmbiguousModelCallTurnIdentities,
    ) -> Result<ReconciliationRequiredToolTurn, ModelCallClosureError>;
    pub fn apply_interrupt_to_retryable_runner_tool_recovery(
        self,
        batch: ToolBatch,
        result_projection: PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: CancelledModelCallTurnIdentities,
    ) -> Result<CancelledModelCallTurn, ModelCallClosureError>;
    pub fn apply_interrupt_to_tool_batch(
        self,
        batch: ToolBatch,
        result_projection: PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: CancelledModelCallTurnIdentities,
    ) -> Result<CancelledModelCallTurn, ModelCallClosureError>;
    pub fn apply_interrupt_to_tool_recovery(
        self,
        wait: AwaitingToolRecovery,
        tool_attempt: EndedToolAttempt,
        result_projection: PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: AmbiguousModelCallTurnIdentities,
    ) -> Result<ReconciliationRequiredToolTurn, ModelCallClosureError>;
    pub fn earliest_queued_turn(&self)
        -> Option<&AcceptedInputTurnSchedulingProjection>;
    pub fn earliest_queued_rendered_base_origins(
        &self,
    ) -> Option<Result<Vec<AcceptedInputId>, ContextFrontierProjectionFailure>>;
    pub fn external_predecessor_rendered_base_origins(
        &self,
        turn: TurnId,
    ) -> Option<Vec<AcceptedInputId>>;
    pub fn resolved_snapshot(
        &self,
        snapshot: ContextFrontierId,
    ) -> Option<&ResolvedContextFrontierSnapshot>;
    pub fn semantic_entry(
        &self,
        entry: SemanticTranscriptEntryRef,
    ) -> Option<&SemanticTranscriptEntry>;
    pub fn prepare_earliest_queued_activation(
        self,
        identities: AcceptedInputTurnActivationIdentities,
    ) -> Result<PreparedAcceptedInputTurnActivation, AcceptedInputEligibilityError>;
    pub fn prepare_active_turn_lost_failure(
        self,
        identities: AcceptedInputTurnFailureIdentities,
    ) -> Result<PreparedAcceptedInputTurnFailure, AcceptedInputTurnFailureError>;
    // accessor: session()
}

pub struct AcceptedInputTurnActivationIdentities { /* private */ }
impl AcceptedInputTurnActivationIdentities {
    pub const fn new(
        model_identity_entry: SemanticTranscriptEntryId,
        origin_entry: SemanticTranscriptEntryId,
        starting_frontier: ContextFrontierId,
        initial_attempt: TurnAttemptId,
    ) -> Self;
    // accessors: model_identity_entry(), origin_entry(), starting_frontier(),
    // initial_attempt()
}

pub struct ActivatedAcceptedInputTurn { /* private */ }
// sealed: PreparedAcceptedInputTurnActivation or checked active scheduling projection
impl ActivatedAcceptedInputTurn {
    // accessors: session(), turn(), accepted_input(), order(), configuration(),
    // configuration_provenance(), start(), phase(), pending_steering(), consumed_steering()
}

pub struct ActivatedDelegatedTurn { /* private */ }
// sealed: PreparedDelegatedTurnActivation
impl ActivatedDelegatedTurn {
    pub fn with_pending_steering(
        self,
        pending_steering: Vec<PendingSteeringInput>,
    ) -> Option<Self>;
    pub fn with_consumed_steering(
        self,
        consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    ) -> Option<Self>;
    // accessors: session(), turn(), spawning_request(), task(), configuration(),
    // delivery_range(), start(), phase(), pending_steering(), consumed_steering()
}

pub enum ActivatedTurn {
    Accepted(ActivatedAcceptedInputTurn),
    Delegated(ActivatedDelegatedTurn),
}
impl ActivatedTurn {
    pub const fn accepted_input(&self) -> Option<&AcceptedInputLifecycle>;
    pub const fn delegated(&self) -> Option<&ActivatedDelegatedTurn>;
    pub fn reconstitute_frontier_entries(
        &self,
        entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    ) -> Option<Vec<SemanticTranscriptEntry>>;
    pub fn apply_automatic_model_call_reconciliation(
        self,
        call: EndedModelCall,
        attempt: EndedTurnAttempt,
        source_snapshot: ResolvedContextFrontierSnapshot,
        recovery_attempt: NonZeroU32,
        identities: AmbiguousModelCallTurnIdentities,
    ) -> Result<ReconciliationRequiredModelCallTurn, ModelCallClosureError>;
    pub fn apply_interrupt_to_runner_recovery(
        self,
        starting_snapshot: ResolvedContextFrontierSnapshot,
        source_snapshot: ResolvedContextFrontierSnapshot,
        result_projection: Option<PreparedToolResultProjection>,
        interrupt: AppliedInterruptCommandResult,
        identities: CancelledModelCallTurnIdentities,
    ) -> Result<CancelledModelCallTurn, ModelCallClosureError>;
    pub fn apply_interrupt_to_runner_tool_recovery(
        self,
        wait: AwaitingToolRecovery,
        tool_attempt: EndedToolAttempt,
        yielded_attempt: TurnAttemptId,
        result_projection: PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: AmbiguousModelCallTurnIdentities,
    ) -> Result<ReconciliationRequiredToolTurn, ModelCallClosureError>;
    pub fn apply_interrupt_to_retryable_runner_tool_recovery(
        self,
        starting_snapshot: ResolvedContextFrontierSnapshot,
        batch: ToolBatch,
        result_projection: PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: CancelledModelCallTurnIdentities,
    ) -> Result<CancelledModelCallTurn, ModelCallClosureError>;
    // accessors: session(), turn(), configuration(), configuration_provenance(),
    // start(), phase(), pending_steering(), consumed_steering()
}

pub struct DelegatedTurnActivationInput {
    pub session: SessionId,
    pub turn: TurnId,
    pub spawning_request: ToolRequestId,
    pub task: DelegationContent,
    pub task_entry: SemanticTranscriptEntryReconstitutionInput,
    pub configuration: OriginConfiguration,
    pub starting_frontier: ContextFrontierId,
    pub initial_attempt: TurnAttemptId,
}

pub struct DelegatedWakeTurnActivationInput {
    pub session: SessionId,
    pub turn: TurnId,
    pub first_delivery_sequence: NonZeroU64,
    pub through_delivery_sequence: NonZeroU64,
    pub deliveries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    pub predecessor: TurnId,
    pub predecessor_snapshot: ResolvedContextFrontierSnapshot,
    pub configuration: OriginConfiguration,
    pub starting_frontier: ContextFrontierId,
    pub initial_attempt: TurnAttemptId,
}

pub struct DelegatedModelCallRecoveryReconstitutionInput { /* private */ }
impl DelegatedModelCallRecoveryReconstitutionInput {
    pub const fn new(
        phase: ActiveTurnSchedulingReconstitutionInput,
        pinned_target: PinnedProviderTargetReconstitutionInput,
        call: ModelCallReconstitutionInput,
        source_snapshot: ResolvedContextFrontierReconstitutionInput,
        pending_steering: Vec<PendingSteeringInput>,
        consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    ) -> Self;
}

pub struct PreparedDelegatedTurnActivation { /* private */ }
// sealed: PreparedDelegatedTurnActivation::prepare
impl PreparedDelegatedTurnActivation {
    pub fn prepare(input: DelegatedTurnActivationInput) -> Option<Self>;
    pub fn prepare_wake(input: DelegatedWakeTurnActivationInput) -> Option<Self>;
    pub fn into_parts(
        self,
    ) -> (
        ActivatedDelegatedTurn,
        Vec<SemanticTranscriptEntry>,
        ResolvedContextFrontierSnapshot,
    );
    pub fn with_reconstituted_phase(
        self,
        phase: ActiveTurnSchedulingReconstitutionInput,
    ) -> Option<(
        ActivatedDelegatedTurn,
        Vec<SemanticTranscriptEntry>,
        ResolvedContextFrontierSnapshot,
    )>;
    pub fn with_reconstituted_model_call_recovery(
        self,
        input: DelegatedModelCallRecoveryReconstitutionInput,
    ) -> Option<(
        ActivatedTurn,
        EndedModelCall,
        EndedTurnAttempt,
        ResolvedContextFrontierSnapshot,
        ResolvedContextFrontierSnapshot,
    )>;
}

pub enum PreparedTurnActivation {
    Accepted(Box<PreparedAcceptedInputTurnActivation>),
    Delegated(Box<PreparedDelegatedTurnActivation>),
}
impl PreparedTurnActivation {
    pub fn turn(&self) -> ActivatedTurn;
    pub fn starting_entries(&self) -> &[SemanticTranscriptEntry];
    pub const fn starting_snapshot(&self) -> &ResolvedContextFrontierSnapshot;
}

pub struct PreparedAcceptedInputTurnActivation { /* private */ }
// sealed: AcceptedInputSchedulingProjection::prepare_earliest_queued_activation
impl PreparedAcceptedInputTurnActivation {
    pub fn into_parts(
        self,
    ) -> (
        ActivatedAcceptedInputTurn,
        Box<[SemanticTranscriptEntry]>,
        ResolvedContextFrontierSnapshot,
    );
    // accessors: turn(), origin_entry(), starting_entries(),
    // starting_snapshot(), start()
}

pub enum AcceptedInputEligibilityFailure {
    ActiveTurnPresent { turn: TurnId },
    ContextCompactionInProgress { call: ModelCallId },
    NoQueuedTurn,
    OriginEntryIdentityAlreadyExists,
    ModelIdentityEntryIdentityAlreadyExists,
    StartingFrontierIdentityAlreadyExists,
    InitialAttemptIdentityAlreadyExists,
    InternalOriginFrontierConstructionFailed,
    InternalPredecessorTerminalFrontierMissing { predecessor: TurnId },
    InternalStartingFrontierDerivationFailed,
}

pub struct AcceptedInputEligibilityError { /* private */ }
// sealed: Err of prepare_earliest_queued_activation
impl AcceptedInputEligibilityError {
    pub fn into_parts(
        self,
    ) -> (
        AcceptedInputSchedulingProjection,
        AcceptedInputTurnActivationIdentities,
        AcceptedInputEligibilityFailure,
    );
    // accessors: projection(), identities(), failure()
}

pub struct AcceptedInputTurnFailureIdentities { /* private */ }
impl AcceptedInputTurnFailureIdentities {
    pub const fn new(
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self;
    pub fn with_pending_steering_reclassifications(
        self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self;
    // accessors: failure_entry(), terminal_frontier(),
    // pending_steering_reclassifications()
}

pub struct FailedAcceptedInputTurn { /* private */ }
// sealed: PreparedAcceptedInputTurnFailure
impl FailedAcceptedInputTurn {
    // accessors: session(), turn(), accepted_input(), order(), start(),
    // ended_attempt(), disposition(), terminal_frontier()
}

pub struct PreparedAcceptedInputTurnFailure { /* private */ }
// sealed: AcceptedInputSchedulingProjection::prepare_active_turn_lost_failure
impl PreparedAcceptedInputTurnFailure {
    pub fn into_parts(
        self,
    ) -> (
        FailedAcceptedInputTurn,
        SemanticTranscriptEntry,
        ResolvedContextFrontierSnapshot,
        Box<[ReclassifiedPendingSteeringTurn]>,
    );
    // accessors: turn(), failure_entry(), terminal_snapshot(),
    // reclassified_pending_steering()
}

pub enum AcceptedInputTurnFailureFailure {
    NoActiveTurn,
    PendingSteeringReclassificationMismatch,
    FailureEntryIdentityAlreadyExists,
    TerminalFrontierIdentityAlreadyExists,
    ActiveAttemptCannotEndLost,
    ActiveStartMissing,
    StartingSnapshotMissing,
    TerminalFrontierCannotAppend,
}

pub struct AcceptedInputTurnFailureError { /* private */ }
// sealed: Err of prepare_active_turn_lost_failure
impl AcceptedInputTurnFailureError {
    pub fn into_parts(
        self,
    ) -> (
        AcceptedInputSchedulingProjection,
        AcceptedInputTurnFailureIdentities,
        AcceptedInputTurnFailureFailure,
    );
    // accessors: projection(), identities(), failure()
}
```

## domain: turn_attempt

```rust
pub struct ProviderTargetMismatchFailureRef { /* private */ }
// sealed: crate-private constructors; trusted producers are the validating
// provider_evidence correlations
impl ProviderTargetMismatchFailureRef {
    // accessors: kind()
}

pub enum ProviderTargetMismatchFailureKind {
    NonterminalCallObservation { evidence: ProviderTargetEvidenceId },
    TerminalAmbiguityResolution { evidence: ProviderTargetEvidenceId },
    TerminalCallInvalidation { invalidated_call: ModelCallId },
}

pub enum AppliedInterruptState {
    NoAppliedInterrupt,
    Applied { proof: AppliedInterruptProof },
}

pub struct FatalMismatchStopCauses { /* private */ }
impl FatalMismatchStopCauses {
    pub fn new(failure: ProviderTargetMismatchFailureRef, interrupt: AppliedInterruptState) -> Self;
    pub fn failures(&self) -> impl ExactSizeIterator<Item = ProviderTargetMismatchFailureRef> + '_;
    pub fn contains(&self, failure: ProviderTargetMismatchFailureRef) -> bool;
    // accessors: interrupt()
}
// nonempty by construction: initialized from one trusted reference

pub enum TurnAttemptStopCauses {
    CancellationOnly { interrupt: AppliedInterruptProof },
    FatalMismatch(FatalMismatchStopCauses),
}
impl TurnAttemptStopCauses {
    pub const fn cancellation_only(interrupt: AppliedInterruptProof) -> Self;
    pub fn fatal_mismatch(failure: ProviderTargetMismatchFailureRef) -> Self;
    pub fn add_fatal_mismatch(self, failure: ProviderTargetMismatchFailureRef) -> Self;
    pub fn add_interrupt(self, proof: AppliedInterruptProof)
        -> Result<Self, TurnAttemptStopCauseUnionError>;
}

pub struct TurnAttemptStopCauseUnionError { /* private */ }
// sealed: Err of TurnAttemptStopCauses::add_interrupt
impl TurnAttemptStopCauseUnionError {
    pub fn into_parts(self) -> (TurnAttemptStopCauses, AppliedInterruptProof);
    // accessors: current(), requested()
}

pub enum AttemptEnd {
    WithoutStop {
        disposition: UnstoppedAttemptDisposition,
    },
    AfterCancellation {
        cause: AppliedInterruptProof,
        disposition: CancellationStopDisposition,
    },
    AfterFatalMismatch {
        causes: FatalMismatchStopCauses,
        disposition: FatalMismatchStopDisposition,
    },
}

pub enum UnstoppedAttemptDisposition {
    TurnCompleted,
    TurnRefused,
    YieldedToDurableWait,
    KnownFailure,
    Lost,
    Ambiguous,
}

pub enum CancellationStopDisposition {
    TurnCompleted,
    TurnRefused,
    KnownFailure,
    Lost,
    Cancelled,
    Ambiguous,
}

pub enum FatalMismatchStopDisposition {
    KnownFailure,
    Lost,
    Ambiguous,
}

pub enum CurrentTurnAttemptState {
    Prepared,
    Running,
    StopRequested { causes: TurnAttemptStopCauses },
}

pub struct CurrentTurnAttempt { /* private */ }
// sealed: the crate-private prepared entry and begin_running are produced by
// the turn_eligibility scheduling seams; the remaining crate-private
// transitions (request_cancellation, request_fatal_mismatch, end_*) stay
// reserved for the turn aggregate
impl CurrentTurnAttempt {
    // accessors: id(), state()
}

pub struct EndedTurnAttempt { /* private */ }
// sealed: crate-private consuming end transitions on CurrentTurnAttempt or
// PreparedDelegatedTurnActivation::with_reconstituted_model_call_recovery;
// exposes no transition back to a current attempt
impl EndedTurnAttempt {
    // accessors: id(), end()
}
```

## domain: model_call

```rust
pub struct ProviderModelIdentity(/* private */);  // identity newtype (see lib.rs shape)

pub struct ResolvedProviderTarget { /* private */ }
impl ResolvedProviderTarget {
    pub const fn naming(identity: ProviderModelIdentity) -> Self;
    // accessors: identity()
}

pub struct PinnedProviderTarget { /* private */ }
// sealed: crate-private constructor reserved for the later resolution-owning
// slice; a raw (turn, target) pair cannot claim a pinned turn fact
impl PinnedProviderTarget {
    // accessors: turn(), target()
}

pub struct PinnedProviderTargetReconstitutionInput { /* private */ }
impl PinnedProviderTargetReconstitutionInput {
    pub const fn new(turn: TurnId, target: ResolvedProviderTarget) -> Self;
    // accessors: turn(), target()
}

pub enum ModelCallDisposition {
    Completed,
    KnownFailed,
    Refused,
    Cancelled,
    Ambiguous,
}

pub enum CurrentModelCallState {
    Prepared,
    InFlight,
    CancellationRequested,
}

pub struct CurrentModelCall { /* private */ }
// sealed: crate-private prepared constructor (consumes the turn's
// PinnedProviderTarget and a ResolvedContextFrontierSnapshot); transitions
// (begin_in_flight, request_cancellation, end_classified,
// end_cancelled_unsent) are crate-private, reserved for the turn aggregate
impl CurrentModelCall {
    // accessors: id(), attempt(), selection(), pinned(), turn(), target(), frontier(), state()
}

pub struct EndedModelCall { /* private */ }
// sealed: crate-private end transitions on CurrentModelCall or
// PreparedDelegatedTurnActivation::with_reconstituted_model_call_recovery;
// terminal — no transition back to a current call
impl EndedModelCall {
    // accessors: id(), attempt(), selection(), pinned(), turn(), target(), frontier(), disposition()
}

pub enum ModelCallReconstitutionState {
    Prepared,
    InFlight,
    CancellationRequested,
    Terminal(ModelCallDisposition),
}

pub struct ModelCallReconstitutionInput { /* private */ }
impl ModelCallReconstitutionInput {
    pub const fn new(
        id: ModelCallId,
        turn: TurnId,
        attempt: TurnAttemptId,
        selection: FrozenModelSelection,
        target: ResolvedProviderTarget,
        frontier: ContextFrontierId,
        state: ModelCallReconstitutionState,
    ) -> Self;
    // accessors: id(), turn(), attempt(), selection(), target(), frontier(), state()
}

pub enum ReconstitutedModelCall {
    Current(CurrentModelCall),
    Ended(EndedModelCall),
}

pub enum ModelCallReconstitutionFailure {
    FrontierMismatch,
    PinnedTargetMismatch,
    InvalidTransition,
}
```

## domain: model_execution

```rust
pub struct ModelTargetDefinition { /* private */ }
impl ModelTargetDefinition {
    pub const fn new(selection: DirectModelSelection, target: ResolvedProviderTarget) -> Self;
    // accessors: selection(), target()
}
pub struct ModelTargetCatalog { /* private */ }
impl ModelTargetCatalog {
    pub fn try_from_definitions(
        definitions: impl IntoIterator<Item = ModelTargetDefinition>,
    ) -> Result<Self, ModelTargetCatalogError>;
    pub fn resolve(
        &self,
        selection: FrozenModelSelection,
    ) -> Result<ResolvedModelSelection, ModelTargetResolutionError>;
}
pub enum ModelTargetCatalogError { DuplicateSelection { selection: DirectModelSelection } }
pub struct ResolvedModelSelection { /* private */ }
// sealed: ModelTargetCatalog::resolve
impl ResolvedModelSelection {
    // accessors: selection(), target()
}
pub struct ModelTargetResolutionError { /* private */ }
impl ModelTargetResolutionError {
    // accessors: selection(), direct_selection()
}
pub struct ModelCallOriginContent { /* private */ }
impl ModelCallOriginContent {
    pub const fn from_goal_turn(accepted_input: AcceptedInputId, content: UserContent) -> Self;
    pub fn from_pending_steering(
        pending: &PendingSteeringInput,
        content: UserContent,
    ) -> Self;
    pub fn from_consumed_steering(
        consumed: &ConsumedSteeringInput,
        content: UserContent,
    ) -> Self;
    pub fn from_recorded_submit(recorded: &ReconstitutedSubmitInput) -> Option<Self>;
    pub fn from_reconstituted_turn_origin(
        origin: &SubmitInputTurnOriginReconstitutionInput,
    ) -> Option<Self>;
    // accessors: accepted_input(), content()
}

pub struct ModelCallExecutionReconstitutionInput { /* private */ }
impl ModelCallExecutionReconstitutionInput {
    pub fn new(
        active_turn: impl Into<ActivatedTurn>,
        targets: ModelTargetCatalog,
        starting_snapshot: ResolvedContextFrontierSnapshot,
        frontier_entries: Vec<SemanticTranscriptEntry>,
        origin_contents: Vec<ModelCallOriginContent>,
        pinned_target: Option<PinnedProviderTargetReconstitutionInput>,
        calls: Vec<ModelCallReconstitutionInput>,
    ) -> Self;
    pub fn with_call_snapshot(
        self,
        call_snapshot: ResolvedContextFrontierReconstitutionInput,
    ) -> Self;
    pub fn with_attachment_blob_facts(
        self,
        facts: Vec<AttachmentBlobFact>,
    ) -> Self;
    pub fn with_continuation_snapshot(
        self,
        continuation_snapshot: ResolvedContextFrontierReconstitutionInput,
    ) -> Self;
    pub fn with_tool_result_correlations(
        self,
        correlations: Vec<ToolResultAttemptCorrelation>,
    ) -> Self;
    pub fn with_tool_denial_correlations(
        self,
        correlations: Vec<ToolApprovalResolution>,
    ) -> Self;
    pub fn with_uncommitted_tool_result_projection(
        self,
        projection: PreparedToolResultProjection,
    ) -> Self;
    pub fn with_availability_successor(self) -> Self;
    pub fn reconstitute(self) -> Result<ModelCallExecution, ModelCallExecutionReconstitutionError>;
}
pub struct ToolResultAttemptCorrelation { /* private */ }
impl ToolResultAttemptCorrelation {
    pub const fn new(
        attempt: ToolAttemptId,
        request: ToolRequestId,
        producing_call: ModelCallId,
    ) -> Self;
    // accessors: attempt(), request(), producing_call()
}
pub enum ModelCallExecutionReconstitutionFailure {
    TurnIsNotRunning,
    StartingSnapshotSessionMismatch,
    StartingSnapshotMismatch,
    CallSnapshotMissing,
    ContinuationSnapshotUnexpected,
    ContinuationSnapshotMismatch,
    CallSnapshotUnexpected,
    CallSnapshotMismatch,
    FrontierEntryMismatch,
    ToolResultCorrelationMismatch,
    ToolDenialCorrelationMismatch,
    MultipleCalls,
    DuplicateOriginContent,
    MissingOriginContent,
    UnreferencedOriginContent,
    AttachmentBlobFactMismatch,
    ConsumedSteeringMismatch,
    CallOwnershipMismatch,
    CallSelectionMismatch,
    CallTargetMismatch,
    PinnedTargetMissing,
    PinnedTargetUnexpected,
    PinnedTargetTurnMismatch,
    InvalidCall,
    LifecycleMismatch,
}
pub struct ModelCallExecutionReconstitutionError { /* private */ }
impl ModelCallExecutionReconstitutionError {
    pub fn into_parts(
        self,
    ) -> (
        ModelCallExecutionReconstitutionInput,
        ModelCallExecutionReconstitutionFailure,
    );
    // accessors: failure(), input()
}

pub struct ModelCallExecution { /* private */ }
impl ModelCallExecution {
    pub fn frontier_entries(
        &self,
    ) -> impl ExactSizeIterator<Item = &SemanticTranscriptEntry>;
    pub fn origin_content(&self, accepted_input: AcceptedInputId) -> Option<&UserContent>;
    pub fn preview_initial_call(
        &self,
        call: ModelCallId,
    ) -> Result<PreparedModelCallRequest, ModelCallPreparationError>;
    pub fn prepare_initial_call(
        self,
        call: ModelCallId,
    ) -> Result<PreparedInitialModelCall, ModelCallPreparationError>;
    pub fn prepare_initial_call_consuming_steering(
        self,
        call: ModelCallId,
        steering_entries: Vec<SemanticTranscriptEntryId>,
        steering_frontier: Option<ContextFrontierId>,
    ) -> Result<PreparedInitialModelCall, ModelCallPreparationError>;
    pub fn resume_prepared_call(&self)
        -> Result<PreparedModelCallRequest, ModelCallResumeFailure>;
    pub fn authorize_send(self) -> Result<AuthorizedModelCall, ModelCallAuthorizationError>;
    pub fn resume_in_flight_call(&self) -> Option<AuthorizedModelCall>;
    pub fn resume_cancellation_requested_call(&self) -> Option<StopRequestedModelCallTurn>;
    pub fn apply_interrupt(
        self,
        interrupt: AppliedInterruptCommandResult,
        identities: CancelledModelCallTurnIdentities,
    ) -> Result<ModelCallInterruptOutcome, ModelCallClosureError>;
    pub fn apply_interrupt_to_tool_batch(
        self,
        interrupt: AppliedInterruptCommandResult,
        result_projection: PreparedToolResultProjection,
        identities: CancelledModelCallTurnIdentities,
    ) -> Result<CancelledModelCallTurn, ModelCallClosureError>;
    pub fn apply_terminal_observation(
        self,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentities,
    ) -> Result<ModelCallTerminalOutcome, ModelCallClosureError>;
    pub fn apply_availability_successor(
        self,
        observation: CorrelatedModelCallTerminalObservation,
        successor_attempt: TurnAttemptId,
    ) -> Result<AvailabilitySuccessorModelCallTurn, ModelCallClosureError>;
    pub fn fail_target_resolution(
        self,
        resolution_error: ModelTargetResolutionError,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError>;
    pub fn fail_credential_pool_exhausted(
        self,
        pool_name: String,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<CredentialPoolExhaustedModelCallTurn, ModelCallClosureError>;
    pub fn fail_automatic_context_compaction(
        self,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError>;
    pub fn fail_prepared_call(
        self,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError>;
    pub fn recover_after_restart(
        self,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<ModelCallTerminalOutcome, ModelCallClosureError>;
    pub fn recover_evidence_free_after_restart(
        self,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError>;
    pub fn recover_tool_crash_after_restart(
        self,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError>;
    pub fn require_context_compaction_after_tool_results(
        self,
        producing_call: ModelCallId,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<ContextHeadroomExhaustedModelCallTurn, ModelCallClosureError>;
    // accessors: active_turn(), session(), turn(), configuration(), start(),
    // current_attempt(), current_call()
}
pub enum ModelCallPreparationFailure {
    TargetUnavailable,
    CallAlreadyExists,
    AttemptIsNotPrepared,
    SteeringIdentityCountMismatch,
    SteeringFrontierIdentityMismatch,
    SteeringCorrelationMismatch,
}
pub struct ModelCallPreparationError { /* private */ }
impl ModelCallPreparationError {
    // accessors: failure(), execution(), target_resolution_error()
}
pub struct PreparedInitialModelCall { /* private */ }
impl PreparedInitialModelCall {
    // accessors: session(), turn(), attempt(), call(), consumed_steering(),
    // steering_snapshot()
}
pub struct PreparedSteeringConsumption { /* private */ }
impl PreparedSteeringConsumption {
    // accessors: accepted_input(), semantic_entry()
}
pub struct PreparedModelCallRequest { /* private */ }
// accessors: session(), turn(), attempt(), dangerous_tool_auto_approval(),
// model_settings(), call(), frontier_entries(), frontier_entry_slice(),
// origin_content(), attachment_byte_length()
pub enum ModelCallResumeFailure { CallMissing, CallIsNotPrepared, AttemptIsNotPrepared }
pub enum ModelCallAuthorizationFailure { CallMissing, CallIsNotPrepared, AttemptIsNotPrepared }
pub struct ModelCallAuthorizationError { /* private */ }
impl ModelCallAuthorizationError {
    // accessors: failure(), execution()
}
pub struct AuthorizedModelCall { /* private */ }
// sealed: ModelCallExecution::authorize_send or resume_in_flight_call
impl AuthorizedModelCall {
    pub fn frontier_entries(
        &self,
    ) -> impl ExactSizeIterator<Item = &SemanticTranscriptEntry>;
    pub fn origin_content(&self, accepted_input: AcceptedInputId) -> Option<&UserContent>;
    // accessors: session(), turn(), attempt(), call(), observation_correlation()
}
pub struct IssuedModelCallCorrelation { /* private */ }
impl IssuedModelCallCorrelation {
    // accessors: session(), turn(), attempt(), call(), target(), frontier()
    pub fn bind_terminal_observation(
        self,
        observation: ModelCallTerminalObservation,
    ) -> CorrelatedModelCallTerminalObservation;
    pub fn bind_terminal_observation_with_usage(
        self,
        observation: ModelCallTerminalObservation,
        usage: ProviderReportedTokenUsage,
    ) -> CorrelatedModelCallTerminalObservation;
    pub fn bind_provider_failure_observation_with_usage(
        self,
        cause: ProviderModelCallFailureCause,
        usage: ProviderReportedTokenUsage,
    ) -> CorrelatedModelCallTerminalObservation;
    pub fn bind_provider_failure_observation_with_retry_after(
        self,
        cause: ProviderModelCallFailureCause,
        usage: ProviderReportedTokenUsage,
        retry_after: Option<std::time::Duration>,
        non_acceptance_proven: bool,
    ) -> CorrelatedModelCallTerminalObservation;
}
pub struct ProviderReportedTokenUsage { /* private */ }
impl ProviderReportedTokenUsage {
    pub const fn unreported() -> Self;
    pub const fn with_input_tokens(self, input_tokens: Option<u64>) -> Self;
    pub const fn with_output_tokens(self, output_tokens: Option<u64>) -> Self;
    pub const fn with_cache_creation_input_tokens(
        self,
        cache_creation_input_tokens: Option<u64>,
    ) -> Self;
    pub const fn with_cache_read_input_tokens(
        self,
        cache_read_input_tokens: Option<u64>,
    ) -> Self;
    // accessors: input_tokens(), output_tokens(),
    // cache_creation_input_tokens(), cache_read_input_tokens()
}
pub enum ProviderModelCallFailureCause {
    CredentialRejected,
    PermissionDenied,
    InvalidRequest,
    TargetNotFound,
    RequestTooLarge,
    RateLimited,
    QuotaExhausted,
    Overloaded,
    ProviderInternal,
    Unrecognized,
}

pub struct CorrelatedModelCallTerminalObservation { /* private */ }
impl CorrelatedModelCallTerminalObservation {
    // accessors: call(), correlation(), observation(), usage(),
    //   provider_failure_cause(), retry_after(), non_acceptance_proven()
}

pub struct AvailabilitySuccessorModelCallTurn { /* private */ }
// sealed: ModelCallExecution::apply_availability_successor
impl AvailabilitySuccessorModelCallTurn {
    // accessors: session(), turn(), predecessor_call(), predecessor_attempt(),
    //   successor_attempt()
}

pub struct CredentialPoolExhaustedModelCallTurn { /* private */ }
// sealed: ModelCallExecution::fail_credential_pool_exhausted
impl CredentialPoolExhaustedModelCallTurn {
    pub fn pool_name(&self) -> &str;
    pub const fn failed(&self) -> &FailedModelCallTurn;
    pub fn into_failed(self) -> FailedModelCallTurn;
}

pub struct ContextHeadroomExhaustedModelCallTurn { /* private */ }
// sealed: ModelCallExecution::require_context_compaction_after_tool_results
impl ContextHeadroomExhaustedModelCallTurn {
    pub const fn producing_call(&self) -> ModelCallId;
    pub const fn failed(&self) -> &FailedModelCallTurn;
    pub fn into_failed(self) -> FailedModelCallTurn;
}

pub enum ModelCallTerminalObservation {
    Completed { assistant_text: Vec<AssistantText> },
    CompletedWithProviderCompaction {
        response: Vec<AssistantResponsePart>,
        retained_input_tokens: u64,
    },
    CompletedWithTools {
        response: ToolUsingAssistantResponse,
        retained_input_tokens: Option<u64>,
    },
    KnownFailed,
    Refused,
    Cancelled,
    Ambiguous,
}
impl ModelCallTerminalObservation {
    // accessors: retained_input_tokens(), disposition()
}
pub struct PendingSteeringReclassificationIdentity { /* private */ }
impl PendingSteeringReclassificationIdentity {
    pub const fn new(accepted_input: AcceptedInputId, turn: TurnId) -> Self;
    // accessors: accepted_input(), turn()
}
pub struct CompletedModelCallIdentities { /* private */ }
impl CompletedModelCallIdentities {
    pub fn new(
        assistant_entries: Vec<SemanticTranscriptEntryId>,
        completion_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self;
    pub fn with_pending_steering_reclassifications(
        self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self;
}
pub enum ToolResponsePartIdentity {
    Text {
        entry: SemanticTranscriptEntryId,
    },
    ProviderCompaction {
        entry: SemanticTranscriptEntryId,
    },
    ToolCall {
        entry: SemanticTranscriptEntryId,
        request: ToolRequestId,
        approval: InitialToolApproval,
    },
}
impl ToolResponsePartIdentity {
    pub const fn text(entry: SemanticTranscriptEntryId) -> Self;
    pub const fn provider_compaction(entry: SemanticTranscriptEntryId) -> Self;
    pub const fn tool_call(
        entry: SemanticTranscriptEntryId,
        request: ToolRequestId,
        approval: InitialToolApproval,
    ) -> Self;
}
pub struct ToolRoundModelCallIdentities { /* private */ }
impl ToolRoundModelCallIdentities {
    pub fn new(
        response_parts: Vec<ToolResponsePartIdentity>,
        yielded_frontier: ContextFrontierId,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Self;
    // accessors: response_parts(), yielded_frontier(), continuation_attempt()
}
pub enum StoppedToolResponsePartIdentity {
    Text {
        entry: SemanticTranscriptEntryId,
    },
    ProviderCompaction {
        entry: SemanticTranscriptEntryId,
    },
    ToolCall {
        entry: SemanticTranscriptEntryId,
        request: ToolRequestId,
        closed_result_entry: SemanticTranscriptEntryId,
        approval: InitialToolApproval,
    },
}
impl StoppedToolResponsePartIdentity {
    pub const fn text(entry: SemanticTranscriptEntryId) -> Self;
    pub const fn provider_compaction(entry: SemanticTranscriptEntryId) -> Self;
    pub const fn tool_call(
        entry: SemanticTranscriptEntryId,
        request: ToolRequestId,
        closed_result_entry: SemanticTranscriptEntryId,
        approval: InitialToolApproval,
    ) -> Self;
}
pub struct StoppedToolRoundModelCallIdentities { /* private */ }
impl StoppedToolRoundModelCallIdentities {
    pub fn new(
        response_parts: Vec<StoppedToolResponsePartIdentity>,
        cancellation_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self;
    pub fn with_pending_steering_reclassifications(
        self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self;
}
pub struct FailedModelCallTurnIdentities { /* private */ }
impl FailedModelCallTurnIdentities {
    pub fn new(
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self;
    pub fn with_pending_steering_reclassifications(
        self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self;
    pub const fn failure_entry(&self) -> SemanticTranscriptEntryId;
    pub const fn terminal_frontier(&self) -> ContextFrontierId;
}
pub struct CancelledModelCallTurnIdentities { /* private */ }
impl CancelledModelCallTurnIdentities {
    pub fn new(
        cancellation_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self;
    pub fn with_pending_steering_reclassifications(
        self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self;
    pub fn into_ambiguous(self) -> AmbiguousModelCallTurnIdentities;
}
pub struct PhysicalCancellationModelCallTurnIdentities { /* private */ }
impl PhysicalCancellationModelCallTurnIdentities {
    pub fn new(
        terminal_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self;
    pub fn with_pending_steering_reclassifications(
        self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self;
}
pub struct RefusedModelCallTurnIdentities { /* private */ }
impl RefusedModelCallTurnIdentities {
    pub fn new(terminal_frontier: ContextFrontierId) -> Self;
    pub fn with_pending_steering_reclassifications(
        self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self;
}
pub struct AmbiguousModelCallTurnIdentities { /* private */ }
impl AmbiguousModelCallTurnIdentities {
    pub const fn new(terminal_frontier: ContextFrontierId) -> Self;
    pub fn with_pending_steering_reclassifications(
        self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self;
}
pub enum ModelCallTerminalIdentities {
    Completed(CompletedModelCallIdentities),
    ToolRound(ToolRoundModelCallIdentities),
    StoppedToolRound(StoppedToolRoundModelCallIdentities),
    Failed(FailedModelCallTurnIdentities),
    PhysicalCancellation(PhysicalCancellationModelCallTurnIdentities),
    Refused(RefusedModelCallTurnIdentities),
    Ambiguous(AmbiguousModelCallTurnIdentities),
}
pub enum ModelCallTerminalOutcome {
    Completed(CompletedModelCallTurn),
    ToolRound(ToolRoundModelCallTurn),
    CancelledWithToolResponse(CancelledToolRoundModelCallTurn),
    Failed(FailedModelCallTurn),
    Cancelled(CancelledModelCallTurn),
    Refused(RefusedModelCallTurn),
    ReconciliationRequired(ReconciliationRequiredModelCallTurn),
    AwaitingRecovery(AmbiguousModelCallTurn),
}
pub enum ModelCallInterruptOutcome {
    Cancelled(CancelledModelCallTurn),
    CancellationRequested(StopRequestedModelCallTurn),
    ReconciliationRequired(ReconciliationRequiredModelCallTurn),
    ToolReconciliationRequired(ReconciliationRequiredToolTurn),
}
pub struct CompletedModelCallTurn { /* private */ }
impl CompletedModelCallTurn {
    // accessors: session(), turn(), call(), attempt(), disposition(),
    // assistant_entries(), completion_entry(), terminal_snapshot(),
    // reclassified_pending_steering()
}
pub struct ToolRoundModelCallTurn { /* private */ }
// accessors: session(), turn(), call(), attempt(), assistant_entries(), requests(),
// automatic_approvals(), yielded_snapshot(), next_phase()
pub struct CancelledToolRoundModelCallTurn { /* private */ }
// accessors: session(), turn(), call(), attempt(), disposition(), assistant_entries(),
// requests(), closed_result_entries(), cancellation_entry(), terminal_snapshot(),
// reclassified_pending_steering()
pub struct FailedModelCallTurn { /* private */ }
impl FailedModelCallTurn {
    // accessors: session(), turn(), optional call(), attempt(), disposition(),
    // failure_entry(), terminal_snapshot(), reclassified_pending_steering()
}
pub struct CancelledModelCallTurn { /* private */ }
// accessors: session(), turn(), call(), optional attempt(), disposition(),
// tool_result_entries(), cancellation_entry(), terminal_snapshot(),
// reclassified_pending_steering()
pub struct StopRequestedModelCallTurn { /* private */ }
// accessors: session(), turn(), call(), attempt(), interrupt(), observation_correlation()
pub struct RefusedModelCallTurn { /* private */ }
impl RefusedModelCallTurn {
    // accessors: session(), turn(), call(), attempt(), disposition(),
    // terminal_snapshot(), reclassified_pending_steering()
}
pub struct ReconciliationRequiredModelCallTurn { /* private */ }
// sealed: AcceptedInputSchedulingProjection::apply_interrupt_to_model_call_recovery,
// AcceptedInputSchedulingProjection::apply_automatic_model_call_reconciliation,
// or ActivatedTurn::apply_automatic_model_call_reconciliation
// accessors: session(), turn(), call(), attempt(), disposition(),
// terminal_snapshot(), reclassified_pending_steering()
pub struct ReconciliationRequiredToolTurn { /* private */ }
// accessors: session(), turn(), tool_attempt(), attempt(), disposition(),
// tool_result_entries(), terminal_snapshot(), reclassified_pending_steering()
pub struct ReclassifiedPendingSteeringTurn { /* private */ }
// sealed: successful model-call terminalization with exact pending identities
impl ReclassifiedPendingSteeringTurn {
    // accessors: session(), source_turn(), accepted_input(), turn(), order(),
    // binding(), effective_configuration()
}
pub struct AmbiguousModelCallTurn { /* private */ }
impl AmbiguousModelCallTurn {
    // accessors: session(), turn(), call(), attempt(), ambiguous_operations()
}
pub enum ModelCallClosureError {
    IdentityShapeMismatch,
    CallStateMismatch,
    ObservationCorrelationMismatch,
    InterruptCorrelationMismatch,
    AttemptStateMismatch,
    TargetResolutionMismatch,
    AssistantIdentityCountMismatch,
    ToolResponseIdentityMismatch,
    ToolRequestOrdinalOverflow,
    InitialToolApprovalMismatch,
    ContinuationAttemptIdentityMismatch,
    PendingSteeringReclassificationMismatch,
    FrontierDerivationFailed,
    AmbiguityConstructionFailed,
}
```

## domain: context_compaction

```rust
pub struct ContextCompactionId(/* private */); // identity newtype (see lib.rs shape)
pub struct ContextCompactionTokenUsage { /* private */ }
impl ContextCompactionTokenUsage {
    pub const fn unreported() -> Self;
    pub const fn with_input_tokens(self, value: Option<u64>) -> Self;
    pub const fn with_output_tokens(self, value: Option<u64>) -> Self;
    pub const fn with_cache_creation_input_tokens(self, value: Option<u64>) -> Self;
    pub const fn with_cache_read_input_tokens(self, value: Option<u64>) -> Self;
    // accessors: input_tokens(), output_tokens(),
    // cache_creation_input_tokens(), cache_read_input_tokens()
}
pub enum ContextCompactionModelCallState {
    Prepared,
    InFlight,
    Terminal(ModelCallDisposition),
}
pub struct ContextCompactionModelCall { /* private */ }
// accessors: id(), session(), selection(), target(), source_frontier(), state(), usage()
pub struct ContextCompactionModelCallReconstitutionInput { /* private */ }
impl ContextCompactionModelCallReconstitutionInput {
    pub const fn new(
        id: ModelCallId,
        session: SessionId,
        selection: DirectModelSelection,
        target: ResolvedProviderTarget,
        source_frontier: ContextFrontierId,
        state: ContextCompactionModelCallState,
        usage: ContextCompactionTokenUsage,
    ) -> Self;
    // accessors: id(), source_snapshot()
    pub fn reconstitute(
        self,
        source: &ResolvedContextFrontierSnapshot,
    ) -> Result<ContextCompactionModelCall, ContextCompactionModelCallReconstitutionFailure>;
}
pub enum ContextCompactionModelCallReconstitutionFailure {
    FrontierMismatch,
    UsageBeforeTerminal,
}
pub struct ContextCompactionRange { /* private */ }
impl ContextCompactionRange {
    pub const fn inclusive(
        first: SemanticTranscriptEntryRef,
        through: SemanticTranscriptEntryRef,
    ) -> Self;
    // accessors: first(), through()
}
pub struct ContextCompaction { /* private */ }
// accessors: id(), session(), predecessor(), source_frontier(), result_frontier(),
// producing_call(), range(), summary_entry()
pub struct ContextCompactionReconstitutionInput { /* private */ }
impl ContextCompactionReconstitutionInput {
    pub const fn new(
        id: ContextCompactionId,
        session: SessionId,
        predecessor: Option<ContextCompactionId>,
        source_frontier: ContextFrontierId,
        result_frontier: ContextFrontierId,
        producing_call: ModelCallId,
        range: ContextCompactionRange,
        summary_entry: SemanticTranscriptEntryId,
    ) -> Self;
    // accessors: id(), source_snapshot(), result_snapshot(), producing_call(),
    // summary_entry()
    pub fn reconstitute(
        self,
        source: &ResolvedContextFrontierSnapshot,
        result: &ResolvedContextFrontierSnapshot,
        source_entries: &[SemanticTranscriptEntry],
        result_entries: &[SemanticTranscriptEntry],
        summary: &SemanticTranscriptEntry,
        call: &ContextCompactionModelCall,
    ) -> Result<ContextCompaction, ContextCompactionReconstitutionFailure>;
}
pub enum ContextCompactionReconstitutionFailure {
    FrontierSessionMismatch,
    FrontierIdentityMismatch,
    FrontierEntryMismatch,
    SourceProjectionInvalid,
    SummaryEntryMismatch,
    SummaryPayloadMismatch,
    RangeEndpointMissing,
    RangeStartMismatch,
    RangeOrderInvalid,
    UnsafeToolExchangeBoundary,
    ResultIsNotSummaryAppend,
    ProducingCallMismatch,
}
pub struct ContextFrontierProjection { /* private */ }
impl ContextFrontierProjection {
    pub fn from_complete_entries(
        entries: &[SemanticTranscriptEntry],
    ) -> Result<Self, ContextFrontierProjectionFailure>;
    pub fn ordered_entries(
        &self,
    ) -> impl ExactSizeIterator<Item = SemanticTranscriptEntryRef> + '_;
}
pub enum ContextFrontierProjectionFailure {
    RangeStartMismatch,
    RangeEndpointMissing,
    RangeOrderInvalid,
    UnsafeToolExchangeBoundary,
    SummaryNotAfterBoundary,
}
```

## domain: context_frontier

```rust
pub struct ContextFrontierId(/* private */);          // identity newtype (see lib.rs shape)
pub struct SemanticTranscriptEntryId(/* private */);  // identity newtype (see lib.rs shape)

pub struct ContextFrontier { /* private */ }
// sealed: ResolvedContextFrontierSnapshot::frontier is the only public producer
impl ContextFrontier {
    // accessors: owning_session(), snapshot()
}

pub struct SemanticTranscriptEntryRef { /* private */ }
impl SemanticTranscriptEntryRef {
    pub const fn from_source(source_session: SessionId, entry: SemanticTranscriptEntryId) -> Self;
    // accessors: source_session(), entry()
}

pub struct ResolvedContextFrontierReconstitutionInput { /* private */ }
// inert input: only the complete scheduling reconstitution seam can consume it
impl ResolvedContextFrontierReconstitutionInput {
    pub fn new(
        owning_session: SessionId,
        snapshot: ContextFrontierId,
        ordered_entries: Vec<SemanticTranscriptEntryRef>,
    ) -> Self;
    pub fn derive_appending(
        &self,
        snapshot: ContextFrontierId,
        appended_entries: Vec<SemanticTranscriptEntryRef>,
    ) -> Self;
    pub fn entry_count(&self) -> usize;
    pub fn reconstitute(self) -> Option<ResolvedContextFrontierSnapshot>;
    // accessors: owning_session(), snapshot(), ordered_entries()
}

pub struct ResolvedContextFrontierSnapshot { /* private */ }
// sealed: crate-private try_from_candidate and derive_appending_candidate,
// consumed by scheduling and model-call aggregate seams
impl ResolvedContextFrontierSnapshot {
    pub fn entry_count(&self) -> usize;
    pub fn appended_entries(
        &self,
    ) -> impl ExactSizeIterator<Item = SemanticTranscriptEntryRef>
           + DoubleEndedIterator
           + '_;
    pub fn immediate_semantic_prefix(&self) -> Option<ContextFrontier>;
    pub fn ordered_entries(
        &self,
    ) -> impl ExactSizeIterator<Item = SemanticTranscriptEntryRef> + DoubleEndedIterator + '_;
    pub fn same_semantic_content(&self, other: &Self) -> bool;
    pub fn is_semantic_prefix_of(&self, later: &Self) -> bool;
    // accessors: frontier()
}
// identity equality (Eq) and semantic-content equality are deliberately
// separate comparisons
```

## domain: semantic_entry

```rust
pub struct AssistantText(/* private */);
impl AssistantText {
    pub fn try_new(value: String) -> Result<Self, NonEmptyUnicodeTextError>;
    pub fn as_str(&self) -> &str;
    pub fn into_string(self) -> String;
}

pub struct ProviderCompactionBlock(/* private */);
impl ProviderCompactionBlock {
    pub fn try_new(value: String) -> Result<Self, ProviderCompactionBlockError>;
    pub fn as_json(&self) -> &str;
    pub fn into_json(self) -> String;
}
pub struct ProviderCompactionBlockError;

pub enum SemanticTranscriptEntryPayload {
    Imported {
        imported_entry: ImportedTranscriptEntryId,
        source_speaker: ImportedSourceAttestation<ImportedSpeaker>,
        content: ImportedTranscriptContent,
    },
    OriginAcceptedInput { accepted_input: AcceptedInputId },
    SteeringAcceptedInput {
        accepted_input: AcceptedInputId,
        source_turn: TurnId,
    },
    DelegatedTask {
        spawning_request: ToolRequestId,
        parent_session: SessionId,
        parent_turn: TurnId,
        content: DelegationContent,
    },
    DelegationMessage {
        spawning_request: ToolRequestId,
        message: DelegationMessageId,
        sender: SessionId,
        recipient: SessionId,
        delivery_sequence: NonZeroU64,
        content: DelegationContent,
    },
    DelegationResult {
        awaiting_request: ToolRequestId,
        spawning_request: ToolRequestId,
        child: SessionId,
        mode: DelegationWaitMode,
        delivery_sequence: Option<NonZeroU64>,
        outcome: Box<DelegationOutcome>,
    },
    ModelIdentityChanged {
        turn: TurnId,
        defaults_version: SessionConfigurationDefaultsVersion,
        selected: DirectModelSelection,
    },
    ContextSummary {
        producing_call: ModelCallId,
        summarized: ContextCompactionRange,
        value: AssistantText,
    },
    TurnFailed { turn: TurnId },
    AssistantText { producing_call: ModelCallId, value: AssistantText },
    ProviderCompaction { producing_call: ModelCallId, block: ProviderCompactionBlock },
    AssistantToolUse { producing_call: ModelCallId, request: ToolRequestId },
    ToolExecutionResult { attempt: ToolAttemptId },
    ToolDenied { request: ToolRequestId },
    ToolClosed { request: ToolRequestId },
    TurnCompleted { turn: TurnId },
    TurnCancelled { turn: TurnId },
}

pub struct SemanticTranscriptEntry { /* private */ }
// sealed: checked scheduling reconstitution plus prepared eligibility and
// model-execution candidates are the only producers
impl SemanticTranscriptEntry {
    // accessors: identity(), source_session(), payload(), reference()
}

pub struct SemanticTranscriptEntryReconstitutionInput { /* private */ }
// inert input: cannot independently construct SemanticTranscriptEntry
impl SemanticTranscriptEntryReconstitutionInput {
    pub fn new(
        identity: SemanticTranscriptEntryId,
        source_session: SessionId,
        payload: SemanticTranscriptEntryPayload,
    ) -> Self;
    // accessors: identity(), source_session(), payload()
}
```

## domain: tool

```rust
pub struct ToolName(/* private */);
impl ToolName {
    pub fn try_new(value: String) -> Result<Self, ToolNameError>;
    pub fn as_str(&self) -> &str;
    pub fn into_string(self) -> String;
}
pub enum ToolNameFailure {
    Empty,
    TooLong { bytes: usize },
    InvalidCharacter { byte_index: usize, character: char },
}
pub struct ToolNameError { /* private */ }
// accessors: value(), failure(), into_parts()

pub enum ToolArgumentsKind {
    Json,
    Undecodable,
}
pub struct NormalizedToolArguments { /* private */ }
impl NormalizedToolArguments {
    pub fn try_from_provider_text(value: String) -> Result<Self, ToolArgumentsError>;
    pub fn try_from_stored(
        kind: ToolArgumentsKind,
        value: String,
    ) -> Result<Self, ToolArgumentsError>;
    // accessors: kind(), as_str(), into_parts()
}
pub enum ToolArgumentsFailure {
    TooLarge { bytes: usize },
    CanonicalTooLarge { bytes: usize },
    ContainsNull,
    CanonicalizationFailed,
    StoredKindMismatch,
    StoredJsonNotCanonical,
}
pub struct ToolArgumentsError { /* private */ }
// accessors: value(), failure(), into_parts()

pub struct ToolRequestOrdinal(/* private u32 */);
impl ToolRequestOrdinal {
    pub fn try_from_usize(value: usize) -> Option<Self>;
    pub const fn from_u32(value: u32) -> Self;
    pub const fn as_u32(self) -> u32;
}
pub struct ToolCallProposal { /* private */ }
impl ToolCallProposal {
    pub const fn new(name: ToolName, arguments: NormalizedToolArguments) -> Self;
    pub fn suppressed(name: ToolName) -> Self;
    // accessors: name(), arguments(), is_suppressed()
}
pub enum AssistantResponsePart {
    Text(AssistantText),
    ProviderCompaction(ProviderCompactionBlock),
    ToolCall(ToolCallProposal),
}
pub struct ToolUsingAssistantResponse { /* private */ }
impl ToolUsingAssistantResponse {
    pub fn try_from_parts(
        parts: Vec<AssistantResponsePart>,
    ) -> Result<Self, ToolUsingAssistantResponseError>;
    // accessors: parts(), tool_count()
}
pub struct ToolUsingAssistantResponseError { /* private */ }
impl ToolUsingAssistantResponseError {
    pub fn into_parts(self) -> Vec<AssistantResponsePart>;
}

pub struct ToolRequest { /* private */ }
// sealed live producer: definitive model-call tool-round transition
impl ToolRequest {
    // accessors: id(), session(), turn(), producing_call(), ordinal(), name(), arguments(), approval_posture()
}
pub struct ToolRequestReconstitutionInput { /* private */ }
impl ToolRequestReconstitutionInput {
    pub const fn new(
        id: ToolRequestId,
        session: SessionId,
        turn: TurnId,
        producing_call: ModelCallId,
        ordinal: ToolRequestOrdinal,
        name: ToolName,
        arguments: NormalizedToolArguments,
    ) -> Self;
    pub const fn with_approval_posture(self, posture: ToolApprovalPosture) -> Self;
    pub fn into_request(self) -> ToolRequest;
}

pub enum DangerousToolAutoApproval {
    Disabled,
    ApproveAll,
}
pub enum ToolPermissionDefault {
    Auto,
    Confirm,
    AlwaysConfirm,
}
pub enum ToolApprovalPosture {
    Auto,
    Delegated,
    Human,
}

pub enum ToolEffectClass {
    EffectFree,
    ExternalEffect,
}
pub enum ToolDecisionSource {
    UserCommand,
    PolicyAuto,
    SessionBlanket,
    SessionOverride,
    Delegate,
    RuntimeSafety,
    LifecycleClosure,
    UserOverride,
}

pub enum ToolApprovalDecider {
    User { command: DurableCommandId },
    Delegate { model: DirectModelSelection, call: ModelCallId },
    UserOverride { command: DurableCommandId, denied_request: ToolRequestId },
}

pub struct ToolDecisionRationale(/* private */);
impl ToolDecisionRationale {
    pub const MAX_UTF8_BYTES: usize;
    pub fn try_new(value: String) -> Result<Self, ToolDecisionRationaleError>;
    pub fn as_str(&self) -> &str;
    pub fn into_string(self) -> String;
}
pub struct ToolDecisionRationaleError { /* private */ }
// accessors: value(), into_value()

pub enum DelegateApprovalRecommendation {
    Approve,
    Deny,
    EscalateToHuman,
}
pub struct DelegateToolApproval { /* private */ }
impl DelegateToolApproval {
    pub fn try_new(
        request: &ToolRequest,
        model: DirectModelSelection,
        call: ModelCallId,
        recommendation: DelegateApprovalRecommendation,
        rationale: ToolDecisionRationale,
    ) -> Result<Self, DelegateToolApprovalError>;
    // accessors: request(), model(), call(), recommendation(), rationale()
}
pub struct DelegateToolApprovalError { /* private */ }
// accessors: posture(), recommendation()

pub struct ToolDenialReason(/* private */);
impl ToolDenialReason {
    pub const MAX_UTF8_BYTES: usize;
    pub fn try_new(value: String) -> Result<Self, ToolDenialReasonError>;
    pub fn from_rationale(rationale: &ToolDecisionRationale) -> Option<Self>;
    pub fn as_str(&self) -> &str;
    pub fn into_string(self) -> String;
}
pub enum ToolDenialReasonFailure {
    Empty,
    TooLong { bytes: usize },
    SurroundingWhitespace,
    ContainsControl,
}
pub struct ToolDenialReasonError { /* private */ }
// accessors: value(), failure(), into_parts()

pub enum ToolApprovalDecision {
    Approve,
    Deny { reason: Option<ToolDenialReason> },
}
pub struct ToolApprovalResolution { /* private */ }
// sealed live producers: user command, registry auto, frozen session blanket,
// checked delegate, consumed user override, provider credential-boundary
// safety denial, or committed lifecycle closure
impl ToolApprovalResolution {
    // accessors: request(), decision(), source(), decider(), rationale(), is_approved()
}
pub struct ToolApprovalResolutionReconstitutionInput { /* private */ }
impl ToolApprovalResolutionReconstitutionInput {
    pub const fn user_command(command: PreparedDecideToolRequest) -> Self;
    pub fn delegate(
        approval: DelegateToolApproval,
        stored_denial_reason: Option<ToolDenialReason>,
    ) -> Self;
    pub const fn policy_auto(request: ToolRequestId) -> Self;
    pub const fn session_blanket(
        request: ToolRequestId,
        frozen_posture: DangerousToolAutoApproval,
    ) -> Self;
    pub const fn runtime_safety(request: ToolRequestId) -> Self;
    pub const fn lifecycle_closure(request: ToolRequestId) -> Self;
    pub const fn user_override(
        request: ToolRequestId,
        command: DurableCommandId,
        denied_request: ToolRequestId,
        frozen_posture: ToolApprovalPosture,
    ) -> Self;
    pub fn reconstitute(
        self,
    ) -> Result<ToolApprovalResolution, ToolApprovalResolutionReconstitutionError>;
}
pub struct ToolApprovalResolutionReconstitutionError { /* private */ }
// accessors: input(), into_input()
pub enum InitialToolApproval {
    Confirm,
    AlwaysConfirm,
    Human,
    Delegated,
    PolicyAuto,
    SessionBlanket,
    RuntimeSafetyDeny,
    UserOverride { command: DurableCommandId, denied_request: ToolRequestId },
}
impl InitialToolApproval {
    pub const fn requires_decision(self) -> bool;
}

pub struct DecideToolRequest { /* private */ }
// canonical equality and hashing exclude command_id
impl DecideToolRequest {
    pub fn try_new(
        command_id: DurableCommandId,
        request: ToolRequestId,
        decision: ToolApprovalDecision,
    ) -> Result<Self, DecideToolRequestConstructionError>;
    pub fn prepare_applied(
        self,
        request: &ToolRequest,
    ) -> Result<PreparedDecideToolRequest, DecideToolRequestPreparationError>;
    pub fn prepare_lifecycle_closure_applied(
        self,
        request: &ToolRequest,
    ) -> Result<PreparedDecideToolRequest, DecideToolRequestPreparationError>;
    pub const fn prepare_request_not_found(self) -> PreparedDecideToolRequest;
    pub const fn prepare_already_resolved(self) -> PreparedDecideToolRequest;
    pub const fn prepare_not_earliest(
        self,
        earliest: ToolRequestId,
    ) -> PreparedDecideToolRequest;
    // accessors: command_id(), request(), decision()
}
pub struct DecideToolRequestConstructionError { /* private */ }
// accessor: command_id()
pub enum DecideToolRequestResult {
    Applied(DecideToolRequestAppliedResult),
    Rejected(DecideToolRequestRejectedResult),
}
pub struct DecideToolRequestAppliedResult { /* private */ }
// accessor: resolution()
pub enum DecideToolRequestRejectedResult {
    RequestNotFound { request: ToolRequestId },
    AlreadyResolved { request: ToolRequestId },
    NotEarliestUndecided {
        request: ToolRequestId,
        earliest: ToolRequestId,
    },
}
pub struct PreparedDecideToolRequest { /* private */ }
// accessors: command(), result(), into_parts()
pub struct DecideToolRequestPreparationError { /* private */ }
// accessors: command(), provided_request(), into_parts()

pub struct RecordedUserOverride { /* private */ }
impl RecordedUserOverride {
    pub const fn new(
        command: DurableCommandId,
        session: SessionId,
        denied_request: ToolRequestId,
        judge_call: ModelCallId,
        tool: ToolName,
        arguments: NormalizedToolArguments,
    ) -> Self;
    pub fn matches_proposal(&self, proposal: &ToolCallProposal) -> bool;
    // accessors: command(), session(), denied_request(), judge_call(), tool(),
    // arguments()
}

pub struct OverrideDeniedToolRequest { /* private */ }
// canonical equality and hashing exclude command_id
impl OverrideDeniedToolRequest {
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        denied_request: ToolRequestId,
    ) -> Result<Self, OverrideDeniedToolRequestConstructionError>;
    pub fn prepare(
        self,
        request: &ToolRequest,
        approval: Option<&ToolApprovalResolution>,
        terminal_resolution: Option<ToolRequestResolution>,
        existing_override_command: Option<DurableCommandId>,
    ) -> Result<PreparedOverrideDeniedToolRequest, OverrideDeniedToolRequestPreparationError>;
    pub fn reconstitute_applied(
        self,
        recorded: RecordedUserOverride,
    ) -> Result<PreparedOverrideDeniedToolRequest, OverrideDeniedToolRequestPreparationError>;
    pub const fn prepare_request_not_found(self) -> PreparedOverrideDeniedToolRequest;
    pub const fn prepare_request_not_in_session(self) -> PreparedOverrideDeniedToolRequest;
    pub const fn prepare_not_delegate_denied(self) -> PreparedOverrideDeniedToolRequest;
    pub const fn prepare_not_terminally_denied(self) -> PreparedOverrideDeniedToolRequest;
    pub const fn prepare_already_overridden(self) -> PreparedOverrideDeniedToolRequest;
    // accessors: command_id(), session(), denied_request()
}
pub struct OverrideDeniedToolRequestConstructionError { /* private */ }
// accessor: command_id()
pub enum OverrideDeniedToolRequestResult {
    Applied(OverrideDeniedToolRequestAppliedResult),
    Rejected(OverrideDeniedToolRequestRejectedResult),
}
pub struct OverrideDeniedToolRequestAppliedResult { /* private */ }
// accessor: recorded()
pub enum OverrideDeniedToolRequestRejectedResult {
    RequestNotFound { denied_request: ToolRequestId },
    RequestNotInSession {
        session: SessionId,
        denied_request: ToolRequestId,
    },
    NotDelegateDenied { denied_request: ToolRequestId },
    NotTerminallyDenied { denied_request: ToolRequestId },
    AlreadyOverridden { denied_request: ToolRequestId },
}
pub struct PreparedOverrideDeniedToolRequest { /* private */ }
// accessors: command(), result(), into_parts()
pub struct OverrideDeniedToolRequestPreparationError { /* private */ }
// accessors: command(), provided_request(), into_parts()

pub enum ToolResultContent {
    Text(ToolResultText),
}
pub struct ToolResultText(/* private */);
impl ToolResultText {
    pub fn try_new(value: String) -> Result<Self, ToolResultTextError>;
    pub fn as_str(&self) -> &str;
    pub fn into_string(self) -> String;
}
pub enum ToolResultTextFailure {
    TooLarge { bytes: usize },
    ContainsNull,
}
pub struct ToolResultTextError { /* private */ }
// accessors: value(), failure(), into_parts()
pub enum ToolRequestResolution {
    Executed { attempt: ToolAttemptId },
    Denied { request: ToolRequestId },
    ClosedByTurnEnd { request: ToolRequestId },
}
```

## domain: tool_attempt

```rust
pub struct ToolDispatchGeneration(/* private u64 */);
impl ToolDispatchGeneration {
    pub const fn try_from_u64(value: u64) -> Option<Self>;
    pub const fn first() -> Self;
    pub const fn checked_next(self) -> Option<Self>;
    pub const fn as_u64(self) -> u64;
}
pub struct ApprovedToolRequest { /* private */ }
impl ApprovedToolRequest {
    pub fn try_from_resolution(
        request: ToolRequest,
        approval: ToolApprovalResolution,
    ) -> Result<Self, ApprovedToolRequestError>;
    pub fn prepare_attempt(
        &self,
        attempt: ToolAttemptId,
        issuing_attempt: TurnAttemptId,
        effect_class: ToolEffectClass,
    ) -> CurrentToolAttempt;
    // accessors: request(), approval()
}
pub struct ApprovedToolRequestError { /* private */ }
// accessors: request(), approval(), into_parts()

pub enum ToolExecutionErrorKind {
    UnknownTool,
    InvalidArguments,
    PreauthorizationRejected,
    ExecutionFailed,
    ResultTooLarge,
    CrashLost,
}
pub struct ToolExecutionErrorDetail(/* private */);
impl ToolExecutionErrorDetail {
    pub fn try_new(value: String) -> Result<Self, ToolExecutionErrorDetailError>;
    pub fn as_str(&self) -> &str;
    pub fn into_string(self) -> String;
}
pub enum ToolExecutionErrorDetailFailure {
    Empty,
    TooLong { bytes: usize },
    SurroundingWhitespace,
    ContainsControl,
}
pub struct ToolExecutionErrorDetailError { /* private */ }
// accessors: value(), failure(), into_parts()
pub struct ToolExecutionError { /* private */ }
impl ToolExecutionError {
    pub const fn new(
        kind: ToolExecutionErrorKind,
        detail: Option<ToolExecutionErrorDetail>,
    ) -> Self;
    // accessors: kind(), detail()
}

pub enum CurrentToolAttemptState {
    Prepared,
    InFlight,
}
pub enum ToolAttemptEnd {
    Completed { result: ToolResultContent },
    KnownFailed { error: ToolExecutionError },
    AwaitingChild {
        spawning_request: ToolRequestId,
        child: SessionId,
    },
    Ambiguous,
}
impl ToolAttemptEnd {
    pub const fn disposition(&self) -> ToolAttemptDisposition;
}
pub enum ToolAttemptDisposition {
    Completed,
    KnownFailed,
    AwaitingChild,
    Ambiguous,
}
pub enum ToolAttemptObservation {
    Completed { result: ToolResultContent },
    KnownFailed { error: ToolExecutionError },
    Ambiguous,
}

pub struct ToolAttemptDispatchCorrelation { /* private */ }
// canonical durable-fact reconstitution plus fence accessors
pub struct ToolAttemptDispatchCorrelationReconstitutionInput {
    /* public complete typed fence facts */
}
impl ToolAttemptDispatchCorrelation {
    pub const fn reconstitute(
        input: ToolAttemptDispatchCorrelationReconstitutionInput,
    ) -> Self;
    // accessors: session(), turn(), issuing_attempt(), request(), attempt(), generation()
}
pub struct IssuedExecutorFence { /* private */ }
impl IssuedExecutorFence {
    pub const fn correlation(&self) -> ToolAttemptDispatchCorrelation;
    pub const fn bind(
        self,
        observation: ToolAttemptObservation,
    ) -> CorrelatedToolAttemptObservation;
}
pub struct CorrelatedToolAttemptObservation { /* private */ }
// accessors: correlation(), observation()
pub struct CurrentToolAttempt { /* private */ }
impl CurrentToolAttempt {
    pub fn end_preflight_error(
        self,
        error: ToolExecutionError,
    ) -> Result<EndedToolAttempt, ToolAttemptTransitionError>;
    pub fn apply_terminal_observation(
        self,
        observation: CorrelatedToolAttemptObservation,
    ) -> Result<EndedToolAttempt, ToolAttemptTransitionError>;
    pub fn end_foreground_child_wait(
        self,
        wait: ChildWait,
    ) -> Result<EndedToolAttempt, ToolAttemptTransitionError>;
    pub fn classify_crash_loss(self) -> ToolAttemptCrashOutcome;
    // accessors: attempt(), request(), session(), turn(), issuing_attempt(), effect_class(),
    // generation(), state()
}
pub struct AuthorizedToolAttempt { /* private */ }
// accessors: attempt(), correlation(), executor_fence(), into_parts()
pub struct ToolDispatchAuthority { /* private */ }
// accessors: request(), attempt(), correlation(), executor_fence()
pub struct EndedToolAttempt { /* private */ }
// accessors: attempt(), request(), session(), turn(), issuing_attempt(), effect_class(),
// generation(), end()
pub enum ToolAttemptCrashOutcome {
    KnownFailed(EndedToolAttempt),
    Ambiguous(EndedToolAttempt),
}
pub enum ToolAttemptTransitionFailure {
    InvalidState,
    CorrelationMismatch,
    InvalidPreflightError,
    InvalidObservationError,
    EffectFreeCannotBeAmbiguous,
    InvalidChildWait,
}
pub struct ToolAttemptTransitionError { /* private */ }
// accessors: attempt(), failure(), into_parts()

pub enum ToolAttemptReconstitutionState {
    Prepared,
    InFlight,
    Ended(ToolAttemptEnd),
}
pub struct ToolAttemptReconstitutionInput { /* private */ }
impl ToolAttemptReconstitutionInput {
    pub const fn new(
        attempt: ToolAttemptId,
        request: ToolRequestId,
        session: SessionId,
        turn: TurnId,
        issuing_attempt: TurnAttemptId,
        effect_class: ToolEffectClass,
        generation: ToolDispatchGeneration,
        state: ToolAttemptReconstitutionState,
    ) -> Self;
    pub fn reconstitute(
        self,
    ) -> Result<ReconstitutedToolAttempt, ToolAttemptReconstitutionError>;
}
pub struct ToolAttemptReconstitutionError { /* private */ }
// accessors: input(), into_input()
pub enum ReconstitutedToolAttempt {
    Current(CurrentToolAttempt),
    Ended(EndedToolAttempt),
}
```

## domain: tool_execution

```rust
pub enum ToolBatchPhaseReconstitutionInput {
    AwaitingApproval { request: ToolRequestId },
    Executing { turn_attempt: TurnAttemptId },
    AwaitingRecovery { attempt: ToolAttemptId },
    AwaitingChild {
        request: ToolRequestId,
        spawning_request: ToolRequestId,
        child: SessionId,
    },
}
pub struct ToolBatchReconstitutionInput { /* private */ }
impl ToolBatchReconstitutionInput {
    pub fn new(
        session: SessionId,
        turn: TurnId,
        producing_call: ModelCallId,
        yielded_snapshot: ResolvedContextFrontierSnapshot,
        requests: Vec<ToolRequest>,
        approvals: Vec<ToolApprovalResolution>,
        attempts: Vec<ReconstitutedToolAttempt>,
        phase: ToolBatchPhaseReconstitutionInput,
    ) -> Self;
    pub fn with_retired_attempts(
        self,
        retired_attempts: Vec<ToolAttemptId>,
    ) -> Self;
    pub fn with_runner_authorized_attempts(
        self,
        runner_authorized_attempts: Vec<ToolAttemptId>,
    ) -> Self;
    pub fn reconstitute(self) -> Result<ToolBatch, ToolBatchReconstitutionError>;
}
pub enum ToolBatchReconstitutionFailure {
    EmptyRequestBatch,
    TooManyRequests,
    RequestOwnershipMismatch,
    RequestOrderMismatch,
    YieldedSnapshotSessionMismatch,
    ApprovalInventoryMismatch,
    AttemptInventoryMismatch,
    AttemptAuthorizationMismatch,
    MultipleLiveAttempts,
    AttemptOrderMismatch,
    ApprovalPhaseMismatch,
    ExecutionPhaseMismatch,
    RecoveryPhaseMismatch,
    ChildWaitPhaseMismatch,
}
pub struct ToolBatchReconstitutionError { /* private */ }
// accessors: input(), failure(), into_parts()
pub enum ToolBatchPhase {
    AwaitingApproval { request: ToolRequestId },
    Executing { turn_attempt: TurnAttemptId },
    AwaitingRecovery { attempt: ToolAttemptId },
    AwaitingChild {
        request: ToolRequestId,
        spawning_request: ToolRequestId,
        child: SessionId,
    },
}

pub struct ToolBatch { /* private */ }
impl ToolBatch {
    pub fn retired_attempts(&self) -> impl Iterator<Item = ToolAttemptId> + '_;
    pub fn runner_authorized_attempts(
        &self,
    ) -> impl Iterator<Item = ToolAttemptId> + '_;
    pub fn awaiting_approval(&self) -> Option<AwaitingToolApproval>;
    pub fn awaiting_recovery(&self) -> Option<AwaitingToolRecovery>;
    pub fn prepare_delegate_decision(
        self,
        approval: DelegateToolApproval,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Result<PreparedDelegateToolApproval, DelegateToolApprovalTransitionError>;
    pub fn prepare_user_decision(
        self,
        command: DecideToolRequest,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Result<PreparedToolBatchDecision, ToolBatchDecisionError>;
    pub fn prepare_lifecycle_closure_denial(
        self,
        command: DecideToolRequest,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Result<PreparedToolBatchDecision, ToolBatchDecisionError>;
    pub fn prepare_next_attempt(
        &self,
        attempt: ToolAttemptId,
        effect_class: ToolEffectClass,
    ) -> Result<PreparedToolAttempt, ToolBatchExecutionError>;
    pub fn authorize_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<AuthorizedToolAttempt, ToolBatchExecutionError>;
    pub fn authorize_dispatch(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<ToolDispatchAuthority, ToolBatchExecutionError>;
    pub fn resume_in_flight_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<AuthorizedToolAttempt, ToolBatchExecutionError>;
    pub fn resume_in_flight_dispatch(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<ToolDispatchAuthority, ToolBatchExecutionError>;
    pub fn authorize_runner_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<RunnerToolAttemptAuthorization, ToolBatchExecutionError>;
    pub fn resume_runner_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<RunnerToolAttemptAuthorization, ToolBatchExecutionError>;
    pub fn prepare_result_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        continuation_frontier: ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError>;
    pub fn prepare_delegation_result_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        continuation_frontier: ContextFrontierId,
        outcome: DelegationOutcome,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError>;
    pub fn prepare_failure_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        result_frontier: ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError>;
    pub fn prepare_cancellation_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        result_frontier: ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError>;
    pub fn prepare_delegation_cancellation_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        result_frontier: ContextFrontierId,
        outcome: Option<DelegationOutcome>,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError>;
    pub fn prepare_reconciliation_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        terminal_frontier: ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError>;
    // accessors: session(), turn(), producing_call(), yielded_snapshot(), requests(),
    // approval(), attempt(), phase()
}
pub struct AwaitingToolApproval { /* private */ }
// sealed: ToolBatch::awaiting_approval
// accessors: session(), turn(), request()
pub struct AwaitingToolRecovery { /* private */ }
// sealed: ToolBatch::awaiting_recovery
// accessors: session(), turn(), producing_call(), yielded_frontier(),
// issuing_attempt(), attempt()
pub struct PreparedDelegateToolApproval { /* private */ }
// accessors: batch(), approval(), resolution(), active_phase()
pub enum DelegateToolApprovalTransitionFailure {
    NoUndecidedRequest,
    RequestMismatch,
    ContinuationAttemptMismatch,
}
pub struct DelegateToolApprovalTransitionError { /* private */ }
// accessors: batch(), approval(), failure()

pub struct PreparedToolBatchDecision { /* private */ }
// accessors: batch(), prepared_command(), active_phase(), into_parts()
pub enum ToolBatchDecisionFailure {
    NoUndecidedRequest,
    CommandCorrelationMismatch,
    ContinuationAttemptMismatch,
}
pub struct ToolBatchDecisionError { /* private */ }
// accessors: batch(), command(), failure()
pub struct PreparedToolAttempt { /* private */ }
// accessors: attempt(), into_attempt()
pub enum ToolBatchExecutionFailure {
    NotExecuting,
    LiveAttemptPresent,
    AttemptMissing,
    AttemptStageMismatch,
    ReadyForContinuation,
    TurnLevelFailure,
    AttemptIdentityReuse,
    ApprovalMismatch,
}
pub struct ToolBatchExecutionError { /* private */ }
// accessor: failure()
pub struct PreparedToolResultProjection { /* private */ }
// accessors: entries(), snapshot(), into_parts()
pub enum ToolResultProjectionFailure {
    BatchNotResolved,
    TurnLevelFailure,
    EntryIdentityReuse,
    FrontierDerivationFailed,
}
pub struct ToolResultProjectionError { /* private */ }
// accessor: failure()
```

## domain: provider_evidence

The module is large but its public surface is deliberately small: the recording
(`record`) and admission (`admit`) mutations, mismatch correlation producers,
and all rejection/outcome types are crate-private seams reserved for the later
aggregate slice.

```rust
pub enum ProviderTargetObservation {
    MatchesResolvedTarget { reported: ProviderModelIdentity },
    Mismatch { reported: ProviderModelIdentity },
}
// an absent reported identity is not representable

pub struct ProviderTargetEvidence { /* private */ }
// sealed: crate-private ProviderTargetEvidenceLog recording boundary
impl ProviderTargetEvidence {
    // accessors: id(), call(), observation()
}

pub struct ProviderTargetEvidenceLog { /* private */ }  // also Default
impl ProviderTargetEvidenceLog {
    pub fn new() -> Self;
    pub fn lookup(&self, id: ProviderTargetEvidenceId) -> Option<&ProviderTargetEvidence>;
    // recording (identifier replay/reuse boundary) is crate-private
}

pub struct ProviderTargetMismatchInvalidation { /* private */ }
// sealed: crate-private ProviderTargetMismatchInvalidationLog admission;
// unique by invalidated call (mismatch invalidation,
// spec/model-call-execution.md)
impl ProviderTargetMismatchInvalidation {
    // accessors: invalidated_call(), first_mismatch_evidence()
}

pub struct ProviderTargetMismatchInvalidationLog { /* private */ }  // also Default
impl ProviderTargetMismatchInvalidationLog {
    pub fn new() -> Self;
    pub fn lookup(&self, call: ModelCallId) -> Option<&ProviderTargetMismatchInvalidation>;
    // admission is crate-private
}
```

## domain: applied_interrupt

Another deliberately tiny public surface: construction of
`AppliedInterruptCommandResult` remains module-private, while the sealed
`SubmitInputTurnOriginAppliedResult::applied_interrupt()` projection exposes the
exact result produced by live preparation or checked reconstitution.

```rust
pub struct AppliedInterruptProof { /* private */ }
// sealed: AppliedInterruptCommandResult::proof is the sole public producer;
// a raw DurableCommandId is never cancellation authority
impl AppliedInterruptProof {
    // accessors: command(), predecessor()
}

pub struct AppliedInterruptCommandResult { /* private */ }
// sealed construction; SubmitInputTurnOriginAppliedResult::applied_interrupt()
// is the sole public projection of a checked applied result
impl AppliedInterruptCommandResult {
    // accessors: proof(), session(), accepted_input(), successor(), successor_order()
}
```

## domain: fatal_mismatch

Zero public items. The entire subtree (`fatal_mismatch.rs`,
`fatal_mismatch/lifecycle.rs`, `fatal_mismatch/prepared.rs` — large) is
`pub(crate)`: post-evidence fact derivation, the reconciliation marker
candidate, and the sealed attempt/turn lifecycle binding are consumed by
`turn_lifecycle` and reserved for the next aggregate slice. Its only externally
visible effect today is that `ReconciliationMarker` (turn_lifecycle) can be
built from its candidate, crate-internally.

## domain: replace_session_defaults

```rust
pub struct ReplaceSessionDefaults { /* private */ }
impl ReplaceSessionDefaults {
    pub fn new(
        command_id: DurableCommandId,
        session: SessionId,
        expected_current_version: SessionConfigurationDefaultsVersion,
        replacement: SessionConfigurationDefaults,
    ) -> Self;
    pub fn with_model_settings(
        command_id: DurableCommandId,
        session: SessionId,
        expected_current_version: SessionConfigurationDefaultsVersion,
        replacement: SessionConfigurationDefaults,
        caller_model_settings: ModelSettingsOverlay,
    ) -> Self;
    pub fn with_model_settings_adjustments(
        command_id: DurableCommandId,
        session: SessionId,
        expected_current_version: SessionConfigurationDefaultsVersion,
        replacement: SessionConfigurationDefaults,
        caller_model_settings: ModelSettingsOverlay,
        model_settings_adjustments: Vec<ModelChangeAdjustment>,
    ) -> Self;
    pub const fn prepare_session_not_found(self) -> PreparedReplaceSessionDefaults;
    pub fn prepare_against(self, current: &Session)
        -> Result<PreparedReplaceSessionDefaults, ReplaceSessionDefaultsPreparationError>;
    // accessors: command_id(), session(), expected_current_version(), replacement(),
    // caller_model_settings(), model_settings_adjustments()
}
// Eq/Hash exclude command_id (comparison-payload rule,
// spec/identity-and-commands.md)

pub enum ReplaceSessionDefaultsResult {
    Applied(ReplaceSessionDefaultsAppliedResult),
    Rejected(ReplaceSessionDefaultsRejectedResult),
}

pub struct ReplaceSessionDefaultsAppliedResult { /* private */ }
// sealed: live preparation (prepare_against) and checked reconstitution
impl ReplaceSessionDefaultsAppliedResult {
    // accessors: session(), installed()
}

pub enum ReplaceSessionDefaultsRejectedResult {
    SessionNotFound(ReplaceSessionDefaultsSessionNotFound),
    CurrentVersionMismatch(ReplaceSessionDefaultsCurrentVersionMismatch),
    VersionExhausted(ReplaceSessionDefaultsVersionExhausted),
}

pub struct ReplaceSessionDefaultsSessionNotFound { /* private */ }
// sealed: prepare_session_not_found and checked reconstitution
impl ReplaceSessionDefaultsSessionNotFound {
    // accessors: session()
}

pub struct ReplaceSessionDefaultsCurrentVersionMismatch { /* private */ }
// sealed: prepare_against and checked reconstitution
impl ReplaceSessionDefaultsCurrentVersionMismatch {
    // accessors: session(), expected(), current()
}

pub struct ReplaceSessionDefaultsVersionExhausted { /* private */ }
// sealed: prepare_against and checked reconstitution
impl ReplaceSessionDefaultsVersionExhausted {
    // accessors: session(), current()
}

pub struct PreparedReplaceSessionDefaults { /* private */ }
// sealed: ReplaceSessionDefaults::prepare_session_not_found / prepare_against
impl PreparedReplaceSessionDefaults {
    pub fn into_parts(self) -> (ReplaceSessionDefaults, ReplaceSessionDefaultsResult);
    // accessors: command(), result()
}

pub struct ReplaceSessionDefaultsPreparationError { /* private */ }
// sealed: Err of prepare_against; adapter correlation failure, not a
// terminal command rejection
impl ReplaceSessionDefaultsPreparationError {
    pub fn into_parts(self) -> (ReplaceSessionDefaults, SessionId);
    // accessors: command(), provided_session()
}

pub struct ReplaceSessionDefaultsReconstitutionInput { /* private */ }
impl ReplaceSessionDefaultsReconstitutionInput {
    pub const fn applied(
        command: ReplaceSessionDefaults,
        result_session: SessionId,
        result_version: SessionConfigurationDefaultsVersion,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    ) -> Self;
    pub const fn rejected_session_not_found(
        command: ReplaceSessionDefaults,
        result_session: SessionId,
    ) -> Self;
    pub const fn rejected_current_version_mismatch(
        command: ReplaceSessionDefaults,
        result_session: SessionId,
        result_expected: SessionConfigurationDefaultsVersion,
        result_current: SessionConfigurationDefaultsVersion,
    ) -> Self;
    pub const fn rejected_version_exhausted(
        command: ReplaceSessionDefaults,
        result_session: SessionId,
        result_current: SessionConfigurationDefaultsVersion,
    ) -> Self;
    pub fn reconstitute(self)
        -> Result<ReconstitutedReplaceSessionDefaults, ReplaceSessionDefaultsReconstitutionError>;
    // accessors: command()
}

pub enum ReplaceSessionDefaultsReconstitutionFailure {
    ResultSessionMismatch,
    DefaultsSessionMismatch,
    ResultVersionMismatch,
    InstalledVersionIsNotSuccessor,
    StoredDefaultsMismatch,
    ResultExpectedVersionMismatch,
    RejectedVersionsAreEqual,
    ResultVersionIsNotExhausted,
}

pub struct ReplaceSessionDefaultsReconstitutionError { /* private */ }
// sealed: Err of ReplaceSessionDefaultsReconstitutionInput::reconstitute
impl ReplaceSessionDefaultsReconstitutionError {
    pub fn into_parts(self) -> (
        ReplaceSessionDefaultsReconstitutionInput,
        ReplaceSessionDefaultsReconstitutionFailure,
    );
    // accessors: failure(), input()
}

pub struct ReconstitutedReplaceSessionDefaults { /* private */ }
// sealed: ReplaceSessionDefaultsReconstitutionInput::reconstitute;
impl ReconstitutedReplaceSessionDefaults {
    // accessors: command(), result()
}
```

## domain: session_lifecycle

```rust
pub enum DispatchingModule {
    RepositoryWatch,
    CommissionedDispatch,
}

pub enum ModuleDispatch {
    RepositoryWatch { dispatch: RepoWatchDispatchId },
    Commissioned { dispatch: CommissionedDispatchId },
}
impl ModuleDispatch {
    pub const fn module(&self) -> DispatchingModule;
}

pub enum CoreAgency {
    Daemon,
    Model { turn: TurnId },
    Tool { request: ToolRequestId },
}

pub enum LifecycleActor {
    Core { agency: CoreAgency },
    Operator,
    Module { module: DispatchingModule },
    Watchdog,
}
impl LifecycleActor {
    pub const fn classify(actor: Actor) -> Self;
}

pub enum SessionOwnership {
    Owned,
    Unmonitored,
}
impl SessionOwnership {
    pub const fn is_owned(&self) -> bool;
}

pub enum SessionWaitKind {
    Approval,
    External,
    Child,
    ProviderRetry,
    Pipeline,
    Scheduler,
}

pub enum SessionWaker {
    ApprovalDecision,
    ExternalRecheck,
    ChildSettlement,
    ProviderBackoff,
    PipelineDrain,
    SchedulerSweep,
}

pub enum SessionWait {
    Approval,
    External,
    Child { session: SessionId },
    ProviderRetry,
    Pipeline,
    Scheduler,
}
impl SessionWait {
    pub const fn kind(&self) -> SessionWaitKind;
    pub const fn waker(&self) -> SessionWaker;
}

pub enum SessionRecoveryOperation {
    ModelCall,
    Tool,
    Runner,
}

pub enum SessionParkCause {
    RetryBudgetExhausted,
    StructuralFailure,
    UnknownFailure,
    ActiveStallDeadlineExpired,
    WaitingDeadlineExpired,
    RecoveringDeadlineExpired,
    OperatorHold,
    ModulePark,
}

impl SessionParkCause {
    pub const fn admits_standing(self, standing: Option<SessionFailureCause>) -> bool;
}

pub enum SessionParkResponder {
    Operator,
    Module { module: DispatchingModule },
}

pub enum SessionRetryableCause {
    ProviderTransient,
    ProviderQuotaExhausted,
    ProviderOverloaded,
    InfrastructureFailure,
    RetryBudgetExhausted,
}

pub enum SessionStructuralCause {
    ContextCompactionWall,
    ContextHeadroomExhausted,
    BrokenToolchain,
    ModerationBlock,
}

pub enum SessionRetirementCause {
    AdmissionDeadlineExpired,
    StrandedQueuedTurn,
}

pub enum SessionFailureCause {
    Retryable(SessionRetryableCause),
    Structural(SessionStructuralCause),
}

pub enum StopStickiness {
    Sticky,
    Redispatchable,
}

pub enum SessionClosureOutcome {
    FailedRetryable,
    FailedStructural,
    FailedUnknown,
    Superseded,
    Abandoned,
    Retired,
}

pub enum SessionTerminalOutcome {
    AchievedVerified,
    AchievedDeclared,
    FailedRetryable { cause: SessionRetryableCause },
    FailedStructural { cause: SessionStructuralCause },
    FailedUnknown,
    Stopped { sticky: StopStickiness },
    Superseded { by: Option<SessionId> },
    Abandoned,
    Retired { cause: SessionRetirementCause },
}
impl SessionTerminalOutcome {
    pub const fn closure_outcome(&self) -> Option<SessionClosureOutcome>;
    pub const fn forbids_further_escalation(&self) -> bool;
    pub const fn releases_resources(&self) -> bool;
}

pub enum SessionLifecycleState {
    Created,
    Dispatched,
    Active,
    Waiting { wait: SessionWait },
    Recovering { operation: SessionRecoveryOperation },
    Blocked { reason: GoalBlockedReasonKind, cycle: u64 },
    Parked {
        cause: SessionParkCause,
        responder: SessionParkResponder,
        standing: Option<SessionFailureCause>,
    },
    Terminal { outcome: SessionTerminalOutcome },
}
impl SessionLifecycleState {
    pub const fn is_terminal(&self) -> bool;
    pub const fn is_parked(&self) -> bool;
    pub const fn admits(&self, next: &Self) -> bool;
    pub fn transition(self, next: Self) -> Result<Self, SessionLifecycleTransitionError>;
}

pub struct SessionLifecycleTransitionError { /* private */ }
impl SessionLifecycleTransitionError {
    // accessors: from(), to()
    // + Display + Error
}

pub enum SessionDeadlineExpiry {
    Retire,
    Park,
}

pub enum SessionDeadlineKind {
    Admission,
    ActiveStall,
    Waiting,
}
impl SessionDeadlineKind {
    pub const fn on_expiry(&self) -> SessionDeadlineExpiry;
    pub const fn for_state(state: &SessionLifecycleState) -> Option<Self>;
}

pub enum SessionOwnershipTransition {
    CreatedOwned,
    CreatedUnmonitored,
    Adopted,
    Released,
}
impl SessionOwnershipTransition {
    pub const fn ownership(&self) -> SessionOwnership;
}
```

## domain: session_lifecycle_command

```rust
pub enum CommandPrincipal {
    Core,
    Operator,
    Module { module: DispatchingModule },
    Watchdog,
}
impl CommandPrincipal {
    pub const fn for_actor(actor: Actor) -> Self;
    pub const fn classify(self, actor: Option<Actor>) -> LifecycleActor;
}

pub enum StartGate {
    Open,
    Held,
}

pub enum FinishCondition {
    ExternalGate,
    Declared(FinishConditionStatement),
}

pub enum FinishCheckVerdict {
    Passed,
    Failed { detail: String },
    Unverified,
}

pub enum SessionLifecycleOperation {
    ReleaseStart,
    Stop { sticky: StopStickiness, descendant_scope: DescendantTerminationScope },
    Supersede { successor: SessionId },
    Abandon,
    CloseFailed { cause: Option<SessionFailureCause> },
    Resume,
    Adopt { finish_condition: Option<FinishCondition> },
    Release,
}

pub struct SessionLifecycleCommand { /* private */ }
impl SessionLifecycleCommand {
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        operation: SessionLifecycleOperation,
    ) -> Self;
    // accessors: command_id(), session(), operation()
}
// Eq/Hash exclude command_id.

pub enum SessionLifecycleCommandRejection {
    SessionNotFound,
    TransitionNotAdmitted,
    RequiresParked,
    ReleaseWhileParked,
    OwnershipUnchanged,
    FinishConditionAlreadyDeclared,
    StandingCauseMismatch,
    SuccessorNotFound,
    SuccessorIsSelf,
    GoalResumeRequired,
    GoalOutcomeMismatch,
    PendingTerminalConflict,
}

pub enum SessionLifecycleApplication {
    StartReleased,
    Closed { outcome: SessionTerminalOutcome },
    ClosurePending {
        outcome: SessionTerminalOutcome,
        live_turn: TurnId,
        defaults_version: SessionConfigurationDefaultsVersion,
    },
    Resumed { state: SessionLifecycleState },
    OwnershipChanged,
}

pub enum SessionLifecycleCommandResult {
    Applied(SessionLifecycleApplication),
    Rejected(SessionLifecycleCommandRejection),
}
```

## domain: session_metadata

```rust
pub struct SessionMetadataContent { /* private */ }
impl SessionMetadataContent {
    pub const MAX_TOTAL_UTF8_BYTES: usize;
    pub const MAX_INDEXED_UTF8_BYTES: usize;
    pub fn empty() -> Self;
    pub fn try_new(
        title: Option<String>,
        tags: Vec<String>,
        attributes: Vec<(String, String)>,
        archived: bool,
    ) -> Result<Self, SessionMetadataContentError>;
    pub fn try_new_with_count_limits(
        title: Option<String>,
        tags: Vec<String>,
        attributes: Vec<(String, String)>,
        archived: bool,
        max_tags: Option<usize>,
        max_attributes: Option<usize>,
    ) -> Result<Self, SessionMetadataContentError>;
    pub fn title(&self) -> Option<&str>;
    pub fn tags(&self) -> impl ExactSizeIterator<Item = &str>;
    pub fn attributes(&self) -> impl ExactSizeIterator<Item = (&str, &str)>;
    pub const fn archived(&self) -> bool;
}

pub enum SessionMetadataContentError {
    EmptyTitle,
    TitleContainsNul,
    TooManyTags,
    EmptyTag,
    TagContainsNul,
    TagExceedsIndexedUtf8Bytes,
    DuplicateTag,
    TooManyAttributes,
    EmptyAttributeKey,
    AttributeKeyContainsNul,
    AttributeKeyExceedsIndexedUtf8Bytes,
    AttributeValueContainsNul,
    DuplicateAttributeKey,
    TotalUtf8BytesExceeded,
}

pub struct SessionMetadataUpdatedAt(/* private */);
impl SessionMetadataUpdatedAt {
    pub const fn from_unix_micros(value: u64) -> Self;
    pub const fn as_unix_micros(self) -> u64;
}

pub struct SessionMetadataLastWriter { /* private */ }
impl SessionMetadataLastWriter {
    pub const fn new(updated_at: SessionMetadataUpdatedAt, actor: Actor) -> Self;
    // accessors: updated_at(), actor()
}

pub struct SessionMetadataSnapshot { /* private */ }
impl SessionMetadataSnapshot {
    pub fn initial(session: SessionId) -> Self;
    pub fn from_recorded_write(
        session: SessionId,
        content: SessionMetadataContent,
        last_writer: SessionMetadataLastWriter,
    ) -> Self;
    // accessors: session(), content(), last_writer()
}

pub struct ReplaceSessionMetadata { /* private */ }
impl ReplaceSessionMetadata {
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        replacement: SessionMetadataContent,
    ) -> Self;
    pub const fn new_for_tool(
        command_id: DurableCommandId,
        session: SessionId,
        request: ToolRequestId,
        replacement: SessionMetadataContent,
    ) -> Self;
    pub fn prepare_session_not_found(self) -> PreparedReplaceSessionMetadata;
    pub fn prepare_applied(
        self,
        updated_at: SessionMetadataUpdatedAt,
    ) -> PreparedReplaceSessionMetadata;
    // accessors: command_id(), session(), actor(), replacement()
}
// Eq/Hash exclude command_id (comparison-payload rule,
// spec/identity-and-commands.md)

pub enum ReplaceSessionMetadataResult {
    Applied(ReplaceSessionMetadataAppliedResult),
    Rejected(ReplaceSessionMetadataRejectedResult),
}

pub struct ReplaceSessionMetadataAppliedResult { /* private */ }
// sealed: live preparation and checked reconstitution
impl ReplaceSessionMetadataAppliedResult {
    // accessor: snapshot()
}

pub enum ReplaceSessionMetadataRejectedResult {
    SessionNotFound(ReplaceSessionMetadataSessionNotFound),
}

pub struct ReplaceSessionMetadataSessionNotFound { /* private */ }
// sealed: prepare_session_not_found and checked reconstitution
impl ReplaceSessionMetadataSessionNotFound {
    // accessor: session()
}

pub struct PreparedReplaceSessionMetadata { /* private */ }
// sealed: ReplaceSessionMetadata preparation methods
impl PreparedReplaceSessionMetadata {
    pub fn into_parts(self) -> (ReplaceSessionMetadata, ReplaceSessionMetadataResult);
    // accessors: command(), result()
}

pub struct ReplaceSessionMetadataReconstitutionInput { /* private */ }
impl ReplaceSessionMetadataReconstitutionInput {
    pub const fn applied(
        command: ReplaceSessionMetadata,
        command_actor: Actor,
        result_session: SessionId,
        result_updated_at: SessionMetadataUpdatedAt,
        result_actor: Actor,
    ) -> Self;
    pub const fn rejected_session_not_found(
        command: ReplaceSessionMetadata,
        command_actor: Actor,
        result_session: SessionId,
    ) -> Self;
    pub fn reconstitute(self)
        -> Result<ReconstitutedReplaceSessionMetadata, ReplaceSessionMetadataReconstitutionError>;
    // accessor: command()
}

pub enum ReplaceSessionMetadataReconstitutionFailure {
    CommandActorMismatch,
    ResultSessionMismatch,
    ResultActorMismatch,
}

pub struct ReplaceSessionMetadataReconstitutionError { /* private */ }
// sealed: Err of ReplaceSessionMetadataReconstitutionInput::reconstitute
impl ReplaceSessionMetadataReconstitutionError {
    pub fn into_parts(self) -> (
        ReplaceSessionMetadataReconstitutionInput,
        ReplaceSessionMetadataReconstitutionFailure,
    );
    // accessors: failure(), input()
}

pub struct ReconstitutedReplaceSessionMetadata { /* private */ }
// sealed: ReplaceSessionMetadataReconstitutionInput::reconstitute
impl ReconstitutedReplaceSessionMetadata {
    // accessors: command(), result()
}
```

## domain: git_remote

```rust
pub const fn max_git_remote_name_bytes() -> usize;
pub const fn max_git_remote_url_bytes() -> usize;

pub enum GitRemoteTextError {
    Empty,
    ContainsNull,
    TooLong { bytes: usize, maximum: usize },
    Malformed,
    UnsupportedScheme,
}
// impl Display + std::error::Error

pub struct GitRemoteName { /* private */ }
impl GitRemoteName {
    pub fn try_new(value: String) -> Result<Self, GitRemoteTextError>;
    // accessors: as_str(), into_string()
}
pub struct GitRemoteUrl { /* private */ }
impl GitRemoteUrl {
    pub fn try_new(value: String) -> Result<Self, GitRemoteTextError>;
    // accessors: as_str(), into_string()
}
// impl Debug redacts the destination
pub struct ConfiguredGitRemoteRecord { /* private */ }
impl ConfiguredGitRemoteRecord {
    pub const fn new(
        mint: GitRemoteMintId,
        workspace: WorkspaceId,
        name: GitRemoteName,
        url: GitRemoteUrl,
    ) -> Self;
    // accessors: mint(), workspace(), name(), url()
}
```

## domain: workspace

```rust
pub enum WorkspaceRootPathError {
    Empty,
    ContainsNull,
    TooLong { bytes: usize, maximum: usize },
    NotAbsolute,
    ContainsControlByte,
    NotCanonical,
    NoFinalComponent,
}
// impl Display + std::error::Error

pub struct WorkspaceRootPath { /* private */ }
impl WorkspaceRootPath {
    pub fn try_new(value: String) -> Result<Self, WorkspaceRootPathError>;
    // accessors: as_str(), into_string()
}

pub enum WorkspaceOrigin {
    OperatorRegistered,
    DaemonDerived,
}
impl WorkspaceOrigin {
    pub const fn is_operator_registered(self) -> bool;
}

pub struct WorkspaceRecord { /* private */ }
impl WorkspaceRecord {
    pub const fn new(
        id: WorkspaceId,
        root: WorkspaceRootPath,
        origin: WorkspaceOrigin,
    ) -> Self;
    // accessors: id(), root(), origin()
}
```

## application: attention

```rust
pub const fn max_attention_snapshot_items() -> u16;
pub const fn max_attention_goal_summary_characters() -> u16;
pub const fn max_attention_change_items() -> u16;
pub const fn max_attention_title_characters() -> u16;
pub const fn max_attention_filter_tags() -> u8;
pub const fn max_attention_filter_utf8_bytes() -> u16;

pub struct AttentionCursor(/* private */);
impl AttentionCursor {
    pub const fn new(value: u64) -> Self;
    pub const fn value(self) -> u64;
}

pub enum AttentionLifecycleState {
    Created,
    Dispatched,
    Active,
    Waiting,
    Recovering,
    Blocked,
    Parked,
    Terminal,
}

pub enum AttentionState {
    Active,
    Queued,
    Blocked,
    AwaitingApproval,
    Ambiguous,
    AwaitingToolRecovery,
    AwaitingReconciliation,
    RunnerLost,
    Parked,
    Idle,
}

pub enum AttentionSort {
    LastActivityDescending,
    SessionIdentityAscending,
}

pub enum AttentionContinuation {
    LastActivity { recorded_at: SystemTime, session: SessionId },
    SessionIdentity(SessionId),
}

pub struct AttentionQuery { /* private */ }
impl AttentionQuery {
    pub fn hot_page() -> Self;
    pub fn identity_page(after: Option<SessionId>) -> Self;
    pub fn try_new(
        search: Option<String>,
        required_tags: Vec<String>,
        include_archived: bool,
        sort: AttentionSort,
        continuation: Option<AttentionContinuation>,
    ) -> Result<Self, AttentionQueryError>;
    pub fn search(&self) -> Option<&str>;
    pub fn required_tags(&self) -> impl ExactSizeIterator<Item = &str>;
    pub const fn include_archived(&self) -> bool;
    pub const fn sort(&self) -> AttentionSort;
    pub const fn continuation(&self) -> Option<&AttentionContinuation>;
}

pub enum AttentionQueryError {
    TooManyTags,
    InvalidTag,
    DuplicateTag,
    InvalidSearch,
    FilterTooLarge,
    ContinuationSortMismatch,
}

pub enum AttentionAction {
    ProvideGoalNeed,
    DecideApproval,
    ReconcileTurn,
}

pub enum AttentionBlockedReason {
    UserInputRequired,
    ExternalChangeRequired,
    AuthorizationRequired,
    ExecutionFailure,
    FinishCheckFailed,
}

pub struct AttentionGoalBlock {
    pub generation: u64,
    pub reason: AttentionBlockedReason,
    pub need_summary: String,
}

pub struct AttentionJudgeFacts {
    pub actionable: u64,
    pub completed: u64,
    pub escalated: u64,
    pub failed: u64,
}

pub struct AttentionActivity {
    pub recorded_at: SystemTime,
    pub kind: AttentionActivityKind,
}

pub enum AttentionActivityKind {
    Session,
    Turn,
    Goal,
    ApprovalJudge,
    Runner,
}

pub struct AttentionSummary {
    pub session: SessionId,
    pub title_summary: Option<String>,
    pub title_truncated: bool,
    pub archived: bool,
    pub current_turn: Option<TurnId>,
    pub active_turn_count: u64,
    pub queued_turn_count: u64,
    pub state: AttentionState,
    pub lifecycle_state: AttentionLifecycleState,
    pub action: Option<AttentionAction>,
    pub goal_block: Option<AttentionGoalBlock>,
    pub judge: AttentionJudgeFacts,
    pub last_activity: AttentionActivity,
}

pub struct AttentionSnapshot {
    pub cursor: AttentionCursor,
    pub total: u64,
    pub sort: AttentionSort,
    pub summaries: Vec<AttentionSummary>,
    pub continuation: Option<AttentionContinuation>,
}

pub enum AttentionChanges {
    Updated {
        cursor: AttentionCursor,
        summaries: Vec<AttentionSummary>,
    },
    ResyncRequired { cursor: AttentionCursor },
}

pub trait AttentionReader {
    type Error;
    fn snapshot(
        &self,
        query: AttentionQuery,
    ) -> impl Future<Output = Result<AttentionSnapshot, Self::Error>> + Send;
    fn changes_after(
        &self,
        cursor: AttentionCursor,
    ) -> impl Future<Output = Result<AttentionChanges, Self::Error>> + Send;
}
```

## application: repo_watch_operations

```rust
pub const fn max_repo_watch_operations_page_items() -> u16;
pub const fn max_repo_watch_activity_page_items() -> u16;

pub struct RepoWatchOperatorEvent {
    pub id: RepoWatchEventId,
    pub cursor_generation: u64,
    pub event_ordinal: u32,
    pub kind: RepoWatchEventKindNameV1,
    pub pull_request: Option<PullRequestNumber>,
    pub observed_at: SystemTime,
}
pub struct RepoWatchOperatorDispatch {
    pub id: RepoWatchDispatchId,
    pub event: RepoWatchEventId,
    pub rule: RepoWatchRuleId,
    pub attempted_at: SystemTime,
}
pub struct RepoWatchOperatorSettlement {
    pub dispatch: RepoWatchDispatchId,
    pub event: RepoWatchEventId,
    pub settled_at: SystemTime,
}
pub struct RepoWatchLatestWebhook {
    pub receipt_sequence: u64,
    pub event_name: String,
    pub action_name: Option<String>,
    pub received_at: SystemTime,
}
pub struct RepoWatchWebhookWindow {
    pub seconds: u32,
    pub received: u64,
    pub projected: u64,
    pub terminal: u64,
    pub quarantined: u64,
}
pub struct RepoWatchEventKindCount {
    pub kind: RepoWatchEventKindNameV1,
    pub count: u64,
}
pub struct RepoWatchRepositoryStatus {
    pub repository: RepositorySlug,
    pub cursor_generation: Option<u64>,
    pub observed_at: Option<SystemTime>,
    pub latest_webhook: Option<RepoWatchLatestWebhook>,
    pub previous_five_minutes: RepoWatchWebhookWindow,
    pub previous_hour: RepoWatchWebhookWindow,
    pub latest_projection_latency_milliseconds: Option<u64>,
    pub maximum_projection_latency_milliseconds_previous_hour: Option<u64>,
    pub event_kind_counts_previous_hour: Vec<RepoWatchEventKindCount>,
    pub last_observed_event: Option<RepoWatchOperatorEvent>,
    pub last_actionable_event: Option<RepoWatchOperatorEvent>,
    pub last_dispatch_attempt: Option<RepoWatchOperatorDispatch>,
    pub last_automation_settlement: Option<RepoWatchOperatorSettlement>,
    pub held_slot_count: u64,
    pub queued_obligation_count: u64,
}
pub struct RepoWatchRepositoryStatusPage {
    pub repositories: Vec<RepoWatchRepositoryStatus>,
    pub continuation_after: Option<RepositorySlug>,
}

pub enum RepoWatchDraftStatus { Draft, ReadyForReview }
pub enum RepoWatchChecksStatus { NoCompletedSuites, Passing, Failing }
pub enum RepoWatchReviewStatus { None, Commented, Approved, ChangesRequested }
pub enum RepoWatchAutomationStatus {
    Unattempted,
    Held { dispatch: RepoWatchDispatchId },
    Queued { latest_event: RepoWatchEventId },
    NonConverged { dispatch: RepoWatchDispatchId },
    StaleSeal {
        dispatch: RepoWatchDispatchId,
        sealed_event: RepoWatchEventId,
    },
    CurrentHeadSealed {
        dispatch: RepoWatchDispatchId,
        sealed_event: RepoWatchEventId,
        settled_at: SystemTime,
    },
}
pub struct RepoWatchPullRequestOperationsFacts {
    pub open_parent: Option<PullRequestNumber>,
    pub open_child_count: u64,
    pub automation: RepoWatchAutomationStatus,
    pub last_observed_event: Option<RepoWatchOperatorEvent>,
    pub last_actionable_event: Option<RepoWatchOperatorEvent>,
    pub last_dispatch_attempt: Option<RepoWatchOperatorDispatch>,
    pub last_automation_settlement: Option<RepoWatchOperatorSettlement>,
    pub held_slot_count: u64,
    pub queued_obligation_count: u64,
    pub commissioned_session_count: u64,
}
pub struct RepoWatchPullRequestOperations {
    pub number: PullRequestNumber,
    pub title: PullRequestTitle,
    pub head: CommitSha,
    pub head_repository: RepositorySlug,
    pub head_branch: BranchName,
    pub base_branch: BranchName,
    pub lifecycle: RepoWatchPullRequestLifecycle,
    pub mergeable: MergeableState,
    pub draft: RepoWatchDraftStatus,
    pub checks: RepoWatchChecksStatus,
    pub review_decision: RepoWatchReviewStatus,
    pub stale_review_count: u64,
    pub unresolved_thread_count: u64,
    pub open_parent: Option<PullRequestNumber>,
    pub open_child_count: u64,
    pub automation: RepoWatchAutomationStatus,
    pub last_observed_event: Option<RepoWatchOperatorEvent>,
    pub last_actionable_event: Option<RepoWatchOperatorEvent>,
    pub last_dispatch_attempt: Option<RepoWatchOperatorDispatch>,
    pub last_automation_settlement: Option<RepoWatchOperatorSettlement>,
    pub held_slot_count: u64,
    pub queued_obligation_count: u64,
    pub commissioned_session_count: u64,
}
impl RepoWatchPullRequestOperations {
    pub fn from_state(
        state: &RepoWatchPullRequestState,
        facts: RepoWatchPullRequestOperationsFacts,
    ) -> Self;
}
pub struct RepoWatchPullRequestPage {
    pub repository: RepositorySlug,
    pub pull_requests: Vec<RepoWatchPullRequestOperations>,
    pub continuation_after: Option<PullRequestNumber>,
}

pub enum RepoWatchHeldSlotBlocker {
    UndeliveredAction,
    DeliveryTurnRuntimeRelevant,
    LiveRuntimeTurn,
    PursuingGoal,
}
pub struct RepoWatchHeldSlot {
    pub dispatch: RepoWatchDispatchId,
    pub singleton: RepoWatchSingletonKey,
    pub rule: RepoWatchRuleId,
    pub held_since: SystemTime,
    pub sessions: Vec<SessionId>,
    pub blockers: Vec<RepoWatchHeldSlotBlocker>,
}
pub enum RepoWatchObligationReadiness {
    Ready,
    Occupied {
        dispatch: RepoWatchDispatchId,
        sessions: Vec<SessionId>,
    },
    ExternallyBlocked { sessions: Vec<SessionId> },
    Cooldown { eligible_at: Option<SystemTime> },
    Parked { parked_at: SystemTime },
}
pub struct RepoWatchObligationId(/* private uuid::Uuid */);
impl RepoWatchObligationId {
    pub const fn from_uuid(value: uuid::Uuid) -> Self;
    pub const fn into_uuid(self) -> uuid::Uuid;
}
pub struct RepoWatchQueuedObligation {
    pub id: RepoWatchObligationId,
    pub singleton: RepoWatchSingletonKey,
    pub rule: RepoWatchRuleId,
    pub first_repository: RepositorySlug,
    pub first_event: RepoWatchEventId,
    pub latest_event: RepoWatchEventId,
    pub matched_event_count: u64,
    pub owed_since: SystemTime,
    pub latest_match_at: SystemTime,
    pub failed_attempts: u64,
    pub readiness: RepoWatchObligationReadiness,
}
pub struct RepoWatchHeldCursor {
    pub held_since: SystemTime,
    pub dispatch: RepoWatchDispatchId,
}
pub struct RepoWatchObligationCursor {
    pub owed_since: SystemTime,
    pub obligation: RepoWatchObligationId,
}
pub enum RepoWatchPagePosition<T> {
    Start,
    After(T),
    Exhausted,
}
pub struct RepoWatchWorkPage {
    pub held_slots: Vec<RepoWatchHeldSlot>,
    pub held_continuation_after: RepoWatchPagePosition<RepoWatchHeldCursor>,
    pub queued_obligations: Vec<RepoWatchQueuedObligation>,
    pub obligation_continuation_after: RepoWatchPagePosition<RepoWatchObligationCursor>,
}

pub enum RepoWatchSessionPurpose {
    RuleDispatch {
        dispatch: RepoWatchDispatchId,
        event: RepoWatchEventId,
        rule: RepoWatchRuleId,
        template: String,
    },
    OperatorCommission {
        dispatch: CommissionedDispatchId,
        template: String,
    },
}
pub struct RepoWatchPullRequestSession {
    pub commissioned_at: SystemTime,
    pub purpose: RepoWatchSessionPurpose,
    pub attention: AttentionSummary,
}
pub struct RepoWatchSessionCursor {
    pub commissioned_at: SystemTime,
    pub session: SessionId,
}
pub struct RepoWatchPullRequestSessionPage {
    pub sessions: Vec<RepoWatchPullRequestSession>,
    pub continuation_before: Option<RepoWatchSessionCursor>,
}

pub struct RepoWatchEventCursor {
    pub cursor_generation: u64,
    pub event_ordinal: u32,
}
pub enum RepoWatchWebhookDisposition {
    Projected,
    Committed,
    DuplicateState,
    Superseded,
    Ignored,
    Quarantined,
}
pub struct RepoWatchWebhookActivity {
    pub receipt_sequence: u64,
    pub event_name: String,
    pub action_name: Option<String>,
    pub received_at: SystemTime,
    pub projection_count: u64,
    pub latest_projected_at: Option<SystemTime>,
    pub disposition: Option<RepoWatchWebhookDisposition>,
}
pub struct RepoWatchActivityPage {
    pub events: Vec<RepoWatchOperatorEvent>,
    pub event_continuation_before: RepoWatchPagePosition<RepoWatchEventCursor>,
    pub webhooks: Vec<RepoWatchWebhookActivity>,
    pub webhook_continuation_before: RepoWatchPagePosition<u64>,
}

pub trait RepoWatchOperationsReader {
    type Error;
    fn repository_statuses(
        &self,
        after: Option<RepositorySlug>,
    ) -> impl Future<Output = Result<RepoWatchRepositoryStatusPage, Self::Error>> + Send;
    fn pull_requests(
        &self,
        repository: RepositorySlug,
        after: Option<PullRequestNumber>,
    ) -> impl Future<Output = Result<RepoWatchPullRequestPage, Self::Error>> + Send;
    fn work(
        &self,
        repository: RepositorySlug,
        held_after: RepoWatchPagePosition<RepoWatchHeldCursor>,
        obligation_after: RepoWatchPagePosition<RepoWatchObligationCursor>,
    ) -> impl Future<Output = Result<RepoWatchWorkPage, Self::Error>> + Send;
    fn pull_request_sessions(
        &self,
        repository: RepositorySlug,
        pull_request: PullRequestNumber,
        before: Option<RepoWatchSessionCursor>,
    ) -> impl Future<Output = Result<RepoWatchPullRequestSessionPage, Self::Error>> + Send;
    fn activity(
        &self,
        repository: RepositorySlug,
        events_before: RepoWatchPagePosition<RepoWatchEventCursor>,
        webhooks_before: RepoWatchPagePosition<u64>,
    ) -> impl Future<Output = Result<RepoWatchActivityPage, Self::Error>> + Send;
}
```

## domain: workspace_instruction

```rust
pub struct InstructionDiscoveryId(Uuid);
pub struct InstructionBundleId(Uuid);
pub struct TurnInstructionManifestId(Uuid);

pub struct InstructionDigest([u8; 32]);
impl InstructionDigest {
    pub fn sha256(bytes: &[u8]) -> Self;
    pub fn source_content(bytes: &[u8]) -> Self;
    pub fn empty_admitted_set() -> Self;
    pub const fn from_sha256(bytes: [u8; 32]) -> Self;
    pub const fn as_bytes(&self) -> &[u8; 32];
}

pub struct InstructionPath { /* private */ }
impl InstructionPath {
    pub fn try_new(value: String) -> Result<Self, InstructionPathError>;
    pub fn as_str(&self) -> &str;
}
pub struct InstructionSourcePathInterner { /* private */ }
impl InstructionSourcePathInterner {
    pub const fn new() -> Self;
    pub fn root_prefix(root_path: InstructionPath) -> InstructionSourcePathPrefix;
    pub fn append_prefix(
        &mut self,
        prefix: &InstructionSourcePathPrefix,
        component: &str,
    ) -> Result<InstructionSourcePathPrefix, InstructionPathError>;
}
pub struct InstructionSourcePathPrefix { /* private */ }
pub struct InstructionSourcePath { /* private */ }
impl InstructionSourcePath {
    pub fn try_new(
        root_path: InstructionPath,
        value: String,
    ) -> Result<Self, InstructionPathError>;
    pub fn try_new_under(
        interner: &mut InstructionSourcePathInterner,
        directory: &InstructionSourcePathPrefix,
        source_name: &str,
    ) -> Result<Self, InstructionPathError>;
    pub fn try_new_in(
        interner: &mut InstructionSourcePathInterner,
        root_path: InstructionPath,
        value: String,
    ) -> Result<Self, InstructionPathError>;
    pub fn absolute_path(&self) -> String;
    pub fn relative_path(&self) -> String;
}
pub enum InstructionPathError {
    Empty,
    ContainsNull,
    TooLong,
    NotAbsolute,
    NotCanonical,
}
pub enum InstructionDiscoveryRootKind { Workspace, Configured }
pub enum InstructionBundleKind { AgentDocument, AgentSkill }

pub struct InstructionSkillMetadataInput {
    pub name: String,
    pub description: String,
    pub parent_directory: String,
}
pub struct InstructionSkillMetadata { /* private */ }
impl InstructionSkillMetadata {
    pub fn try_new(
        input: InstructionSkillMetadataInput,
    ) -> Result<Self, InstructionSkillMetadataError>;
    // accessors: name(), description()
}
pub enum InstructionSkillMetadataError {
    InvalidName,
    InvalidDescription,
    ParentMismatch,
}

pub struct InstructionBundleRegistrationInput {
    pub kind: InstructionBundleKind,
    pub root_kind: InstructionDiscoveryRootKind,
    pub root_path: InstructionPath,
    pub source_path: InstructionSourcePath,
    pub source_bytes: u64,
    pub source_hash: InstructionDigest,
    pub skill: Option<InstructionSkillMetadata>,
}
pub struct InstructionBundleRegistration { /* private */ }
impl InstructionBundleRegistration {
    pub fn new(input: InstructionBundleRegistrationInput) -> Option<Self>;
    // accessors: kind(), root_kind(), root_path(), source_path(),
    // relative_source_path(), agent_document_scope(), source_bytes(), source_hash(), skill()
}

pub struct EmptyTurnInstructionManifestEvidence {
    pub eligibility_hash: InstructionDigest,
    pub admitted_set_hash: InstructionDigest,
    pub manifest_hash: InstructionDigest,
}
pub struct TurnInstructionManifest { /* private */ }
impl TurnInstructionManifest {
    pub fn empty_turn_start(
        id: TurnInstructionManifestId,
        session: SessionId,
        turn: TurnId,
    ) -> Self;
    pub fn reconstitute_empty_turn_start(
        id: TurnInstructionManifestId,
        session: SessionId,
        turn: TurnId,
        evidence: EmptyTurnInstructionManifestEvidence,
    ) -> Option<Self>;
    // accessors: id(), session(), turn(), eligibility_hash(), admitted_set_hash(),
    // manifest_hash()
}
```

## application: approval_judge

```rust
pub enum ApprovalJudgeDispatchProvenance {
    RepoWatch(RepoWatchDispatchId),
    Commissioned(CommissionedDispatchId),
}
impl ApprovalJudgeDispatchProvenance {
    pub const fn into_uuid(self) -> uuid::Uuid;
}

pub enum ApprovalJudgeDispatchAuthority {
    PullRequest(ApprovalJudgePullRequestAuthority),
    Branch(ApprovalJudgeBranchAuthority),
}
impl ApprovalJudgeDispatchAuthority {
    // accessor: dispatch()
}

pub struct ApprovalJudgePullRequestAuthorityInput {
    pub dispatch: ApprovalJudgeDispatchProvenance,
    pub repository: RepositorySlug,
    pub pull_request: PullRequestNumber,
    pub head_sha: CommitSha,
    pub head_repository: RepositorySlug,
    pub head_branch: BranchName,
    pub base_branch: BranchName,
}

pub struct ApprovalJudgePullRequestAuthority { /* private */ }
impl ApprovalJudgePullRequestAuthority {
    pub const fn new(input: ApprovalJudgePullRequestAuthorityInput) -> Self;
    // accessors: dispatch(), repository(), pull_request(), head_sha(), head_repository(), head_branch(), base_branch()
}

pub struct ApprovalJudgeBranchAuthorityInput {
    pub dispatch: ApprovalJudgeDispatchProvenance,
    pub repository: RepositorySlug,
    pub branch: BranchName,
}

pub struct ApprovalJudgeBranchAuthority { /* private */ }
impl ApprovalJudgeBranchAuthority {
    pub const fn new(input: ApprovalJudgeBranchAuthorityInput) -> Self;
    // accessors: dispatch(), repository(), branch()
}

pub struct ApprovalJudgeCompletionIdentities { /* private */ }
impl ApprovalJudgeCompletionIdentities {
    pub const fn new(
        continuation_attempt: TurnAttemptId,
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self;
    // accessors: continuation_attempt(), failure_entry(), terminal_frontier()
}

pub trait ApprovalJudgeAuthorization {
    fn request(&self) -> &ToolRequest;
    fn call(&self) -> ModelCallId;
    fn selection(&self) -> DirectModelSelection;
    fn target(&self) -> ResolvedProviderTarget;
    fn credential_reference(&self) -> &str;
}
```

## application: blob_derivation

```rust
pub trait BlobDerivationIdGenerator {
    fn next_blob_derivation_id(&mut self) -> BlobDerivationId;
}

pub struct UuidV7BlobDerivationIdGenerator;

pub trait BlobDerivationStore {
    type Error;
    fn find_deterministic(
        &self,
        key: DeterministicBlobDerivationKey,
    ) -> impl Future<Output = Result<Option<BlobDerivation>, Self::Error>> + Send;
    fn record_deterministic(
        &self,
        key: DeterministicBlobDerivationKey,
        derivation: BlobDerivation,
    ) -> impl Future<Output = Result<BlobDerivationRecordOutcome, Self::Error>> + Send;
}

pub enum BlobDerivationRecordOutcome {
    Recorded(BlobDerivation),
    Existing(BlobDerivation),
}

pub trait DeterministicBlobProducer {
    type Error;
    fn produce(
        &mut self,
        inputs: &[BlobDigest],
        transformation: &BlobTransformation,
    ) -> impl Future<Output = Result<Box<[BlobDigest]>, Self::Error>> + Send;
    fn outputs_retrievable(
        &mut self,
        outputs: &[BlobDigest],
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
}

pub struct DeterministicBlobDerivationRequest { /* private */ }
impl DeterministicBlobDerivationRequest {
    pub fn try_new(
        inputs: impl Into<Box<[BlobDigest]>>,
        transformation: BlobTransformation,
        implementation: BlobDigest,
    ) -> Result<Self, BlobDerivationError>;
    // accessors: inputs(), transformation(), implementation(), key()
}

pub enum BlobDerivationServiceOutcome {
    Reused(BlobDerivation),
    Produced(BlobDerivation),
}

pub enum BlobDerivationServiceError<StoreError, ProducerError> {
    Store(StoreError),
    Producer(ProducerError),
    InvalidProducerOutput(BlobDerivationError),
}

pub struct DeterministicBlobDerivationService<Ids, Store, Producer> { /* private */ }
impl<Ids, Store, Producer> DeterministicBlobDerivationService<Ids, Store, Producer> {
    pub const fn new(ids: Ids, store: Store, producer: Producer) -> Self;
    pub async fn execute(
        &mut self,
        request: DeterministicBlobDerivationRequest,
    ) -> Result<
        BlobDerivationServiceOutcome,
        BlobDerivationServiceError<Store::Error, Producer::Error>,
    >;
}
```

## application: commissioned_dispatch

```rust
pub enum CommissionedDispatchFence {
    PullRequest {
        repository: RepositorySlug,
        pull_request: PullRequestNumber,
        head_sha: CommitSha,
        head_repository: RepositorySlug,
        head_branch: BranchName,
        base_branch: BranchName,
    },
    Branch {
        repository: RepositorySlug,
        branch: BranchName,
    },
}

pub trait CommissionedDispatchIdGenerator {
    fn next_dispatch_id(&mut self) -> CommissionedDispatchId;
    fn next_command_id(&mut self) -> DurableCommandId;
    fn next_session_id(&mut self) -> SessionId;
}

pub struct UuidV7CommissionedDispatchIdGenerator;

pub struct CommissionDispatchRequest { /* private */ }
impl CommissionDispatchRequest {
    pub fn try_new(
        command_id: DurableCommandId,
        template: SessionTemplateName,
        fence: CommissionedDispatchFence,
        statement: GoalStatement,
        context: UserContent,
    ) -> Result<Self, InvalidDurableCommandId>;
    pub fn prepare(
        self,
        ids: &mut impl CommissionedDispatchIdGenerator,
        template_provenance: SessionTemplateProvenance,
        resolved_defaults: SessionConfigurationDefaults,
    ) -> Result<PreparedCommissionedDispatch, CommissionDispatchPreparationError>;
    // accessors: command_id(), template(), fence(), statement(),
    //            initial_content_digest()
}

// sealed: CommissionDispatchRequest::prepare
pub struct PreparedCommissionedDispatch { /* private */ }
impl PreparedCommissionedDispatch {
    pub fn into_parts(
        self,
    ) -> (
        CommissionedDispatchId,
        CommissionedDispatchFence,
        PreparedCreateSession,
        SubmitInput,
        GoalUserCommand,
    );
    // accessors: dispatch_id(), fence(), prepared_session(), goal(), session(),
    //            initial_content_digest()
}

pub enum CommissionDispatchPreparationError {
    TemplateMismatch,
    SessionPreparation,
}
```

## application: conversation_import

```rust
pub trait ImportedConversationIdGenerator {
    fn next_conversation_id(&mut self) -> ImportedConversationId;
    fn next_entry_id(&mut self) -> ImportedTranscriptEntryId;
}

pub struct UuidV7ImportedConversationIdGenerator;
// Default; impl ImportedConversationIdGenerator

pub trait ImportedConversationConverter {
    type Error;
    fn format(&self) -> ImportedConversationFormat;
    fn convert<NextEntryId>(
        &mut self,
        conversation: ImportedConversationId,
        source: &[u8],
        next_entry_id: NextEntryId,
    ) -> Result<ImportedConversation, Self::Error>
    where
        NextEntryId: FnMut() -> ImportedTranscriptEntryId;
}

pub struct ImportedConversationSkippedRecord<Failure> { /* private */ }
impl<Failure> ImportedConversationSkippedRecord<Failure> {
    pub const fn new(source_line: u64, failure: Failure) -> Self;
    // accessors: source_line(), failure(), into_parts()
}

pub enum ImportedConversationConversionReport<Failure> {
    Converted {
        conversation: ImportedConversation,
        skipped_records: Box<[ImportedConversationSkippedRecord<Failure>]>,
    },
    NoValidRecords {
        skipped_records: Box<[ImportedConversationSkippedRecord<Failure>]>,
    },
}

pub trait ResilientImportedConversationConverter:
    ImportedConversationConverter
{
    type RecordFailure;
    fn convert_resilient<NextEntryId>(
        &mut self,
        conversation: ImportedConversationId,
        source: &[u8],
        next_entry_id: NextEntryId,
    ) -> Result<
        ImportedConversationConversionReport<Self::RecordFailure>,
        Self::Error,
    >
    where
        NextEntryId: FnMut() -> ImportedTranscriptEntryId;
}

pub enum ImportedConversationStoreOutcome {
    Inserted {
        conversation: ImportedConversationId,
        source_digest: ImportedConversationSourceDigest,
    },
    AlreadyImported {
        conversation: ImportedConversationId,
        source_digest: ImportedConversationSourceDigest,
    },
}
impl ImportedConversationStoreOutcome {
    // accessors: conversation(), source_digest()
}

pub trait ImportedConversationStore {
    type Error;
    fn resolve_or_insert(
        &mut self,
        conversation: ImportedConversation,
    ) -> impl Future<
        Output = Result<ImportedConversationStoreOutcome, Self::Error>,
    > + Send;
}

pub enum ImportConversationOutcome {
    Inserted {
        conversation: ImportedConversationId,
    },
    AlreadyImported {
        conversation: ImportedConversationId,
    },
}
impl ImportConversationOutcome {
    // accessor: conversation()
}

pub enum ImportConversationReport<Failure> {
    Converted {
        conversation: ImportedConversation,
        skipped_records: Box<[ImportedConversationSkippedRecord<Failure>]>,
    },
    Imported {
        outcome: ImportConversationOutcome,
        skipped_records: Box<[ImportedConversationSkippedRecord<Failure>]>,
    },
    NoValidRecords {
        skipped_records: Box<[ImportedConversationSkippedRecord<Failure>]>,
    },
}

pub enum ImportConversationError<ConverterError, StoreError> {
    Conversion(ConverterError),
    ConverterIdentityMismatch {
        supplied: ImportedConversationId,
        converted: ImportedConversationId,
    },
    ConverterFormatMismatch {
        declared: ImportedConversationFormat,
        converted: ImportedConversationFormat,
    },
    ConverterEntryIdentitySequenceMismatch,
    StoreSourceDigestMismatch {
        expected: ImportedConversationSourceDigest,
        actual: ImportedConversationSourceDigest,
    },
    StoreInsertedIdentityMismatch {
        expected: ImportedConversationId,
        actual: ImportedConversationId,
    },
    Store(StoreError),
}
// impl Display + std::error::Error (bounded on both adapter errors)

pub struct ImportConversationService<Generator, Converter, Store> { /* private */ }
impl<Generator, Converter, Store>
    ImportConversationService<Generator, Converter, Store>
{
    pub const fn new(ids: Generator, converter: Converter, store: Store) -> Self;
    pub fn into_parts(self) -> (Generator, Converter, Store);
}
impl<
    Generator: ImportedConversationIdGenerator,
    Converter: ImportedConversationConverter,
    Store: ImportedConversationStore,
> ImportConversationService<Generator, Converter, Store>
{
    pub async fn execute(
        &mut self,
        source: &[u8],
    ) -> Result<
        ImportConversationOutcome,
        ImportConversationError<Converter::Error, Store::Error>,
    >;
}
impl<
    Generator: ImportedConversationIdGenerator,
    Converter: ResilientImportedConversationConverter,
    Store: ImportedConversationStore,
> ImportConversationService<Generator, Converter, Store>
{
    pub async fn execute_resilient(
        &mut self,
        source: &[u8],
    ) -> Result<
        ImportConversationReport<Converter::RecordFailure>,
        ImportConversationError<Converter::Error, Store::Error>,
    >;
}
```

## application: create_session

```rust
pub enum InvalidDurableCommandId {
    Nil,
    Max,
}
// impl Display + std::error::Error

pub struct CreateSessionRequest { /* private */ }
impl CreateSessionRequest {
    pub fn try_new(
        command_id: DurableCommandId,
        initial_configuration_defaults: SessionConfigurationDefaults,
    ) -> Result<Self, InvalidDurableCommandId>;
    pub fn try_new_from_template(
        command_id: DurableCommandId,
        template_provenance: SessionTemplateProvenance,
        resolved_configuration_defaults: SessionConfigurationDefaults,
    ) -> Result<Self, InvalidDurableCommandId>;
    pub fn with_placement(self, placement: SessionPlacement) -> Self;
    pub fn with_lifecycle(
        self,
        start_gate: StartGate,
        ownership: SessionOwnership,
        finish_condition: Option<FinishCondition>,
    ) -> Self;
    // accessors: command_id(), initial_configuration_defaults(), template_provenance(),
    //   placement(), start_gate(), ownership(), finish_condition()
}

pub trait SessionIdGenerator {
    fn next_session_id(&mut self) -> SessionId;
}

pub struct UuidV7SessionIdGenerator;  // Default; impl SessionIdGenerator

pub enum CreateSessionOutcome {
    Applied(CreateSessionAppliedResult),
    ConflictingReuse { command_id: DurableCommandId },
}

pub trait CreateSessionTransaction {
    type Error;

    fn handle(
        &mut self,
        prepared: PreparedCreateSession,
    ) -> impl Future<Output = Result<CreateSessionOutcome, Self::Error>> + Send;
}

pub enum CreateSessionError<TransactionError> {
    Preparation(CreateSessionPreparationFailure),
    Transaction(TransactionError),
}
// impl Display + std::error::Error (bounded on TransactionError)

pub struct CreateSessionService<Generator, Transaction> { /* private */ }
impl<Generator, Transaction> CreateSessionService<Generator, Transaction> {
    pub const fn new(session_ids: Generator, transaction: Transaction) -> Self;
    pub fn into_parts(self) -> (Generator, Transaction);
}
impl<Generator: SessionIdGenerator, Transaction: CreateSessionTransaction>
    CreateSessionService<Generator, Transaction>
{
    pub async fn execute(
        &mut self,
        request: CreateSessionRequest,
    ) -> Result<CreateSessionOutcome, CreateSessionError<Transaction::Error>>;
}
```

## application: update_session_placement

```rust
pub struct UpdateSessionPlacementRequest { /* private */ }
impl UpdateSessionPlacementRequest {
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        expected_version: SessionPlacementVersion,
        replacement: SessionPlacement,
    ) -> Result<Self, InvalidDurableCommandId>;
}

pub trait UpdateSessionPlacementTransaction {
    type Error;
    fn handle(
        &mut self,
        command: UpdateSessionPlacement,
    ) -> impl Future<Output = Result<UpdateSessionPlacementOutcome, Self::Error>> + Send;
}

pub enum UpdateSessionPlacementOutcome {
    Recorded(UpdateSessionPlacementResult),
    ConflictingReuse { command_id: DurableCommandId },
}

pub struct UpdateSessionPlacementService<Transaction> { /* private */ }
impl<Transaction> UpdateSessionPlacementService<Transaction> {
    pub const fn new(transaction: Transaction) -> Self;
}
impl<Transaction: UpdateSessionPlacementTransaction>
    UpdateSessionPlacementService<Transaction>
{
    pub async fn execute(
        &mut self,
        request: UpdateSessionPlacementRequest,
    ) -> Result<UpdateSessionPlacementOutcome, Transaction::Error>;
}
```

## application: create_session_from_imported_frontier

```rust
pub struct CreateSessionFromImportedFrontierRequest { /* private */ }
impl CreateSessionFromImportedFrontierRequest {
    pub fn try_new(
        command_id: DurableCommandId,
        imported_frontier: ImportedTranscriptFrontier,
        relationship: ImportedSessionRelationship,
        initial_configuration_defaults: SessionConfigurationDefaults,
    ) -> Result<Self, InvalidDurableCommandId>;
    // accessors: command_id(), imported_frontier(), relationship(),
    //   initial_configuration_defaults()
}

pub trait CreateSessionFromImportedFrontierIdGenerator {
    fn next_session_id(&mut self) -> SessionId;
    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId;
    fn next_context_frontier_id(&mut self) -> ContextFrontierId;
}

pub struct UuidV7CreateSessionFromImportedFrontierIdGenerator;
// Default; impl CreateSessionFromImportedFrontierIdGenerator

pub enum CreateSessionFromImportedFrontierOutcome {
    Applied(CreateSessionFromImportedFrontierAppliedResult),
    ImportedConversationNotFound {
        conversation: ImportedConversationId,
    },
    ImportedFrontierNotFound {
        frontier: ImportedTranscriptFrontier,
    },
    ConflictingReuse {
        command_id: DurableCommandId,
    },
}

pub trait CreateSessionFromImportedFrontierTransaction {
    type Error;

    fn handle<NextSemanticEntryId>(
        &mut self,
        command: CreateSessionFromImportedFrontier,
        session: SessionId,
        seed_frontier: ContextFrontierId,
        next_semantic_entry_id: NextSemanticEntryId,
    ) -> impl Future<
        Output = Result<CreateSessionFromImportedFrontierOutcome, Self::Error>,
    > + Send
    where
        NextSemanticEntryId: FnMut() -> SemanticTranscriptEntryId + Send;
}

pub struct CreateSessionFromImportedFrontierService<Generator, Transaction> {
    /* private */
}
impl<Generator, Transaction>
    CreateSessionFromImportedFrontierService<Generator, Transaction>
{
    pub const fn new(ids: Generator, transaction: Transaction) -> Self;
    pub fn into_parts(self) -> (Generator, Transaction);
}
impl<
    Generator: CreateSessionFromImportedFrontierIdGenerator + Send,
    Transaction: CreateSessionFromImportedFrontierTransaction,
> CreateSessionFromImportedFrontierService<Generator, Transaction>
{
    pub async fn execute(
        &mut self,
        request: CreateSessionFromImportedFrontierRequest,
    ) -> Result<CreateSessionFromImportedFrontierOutcome, Transaction::Error>;
}
```

## application: list_conversations

```rust
pub enum ConversationOriginFilter {
    Native,
    Imported,
    All,
}
impl ConversationOriginFilter {
    pub const fn selects_native(self) -> bool;
    pub const fn selects_imported(self) -> bool;
}

pub enum ConversationListCursor {
    NativeSession(SessionId),
    ImportedConversation(ImportedConversationId),
}
impl ConversationListCursor {
    pub const fn identity_uuid(self) -> uuid::Uuid;
}

pub struct ConversationListQuery { /* private */ }
impl ConversationListQuery {
    pub fn default_page(page_size: u64) -> Self;
    pub fn try_new(
        title_contains: Option<String>,
        origin: ConversationOriginFilter,
        include_archived: bool,
        page_size: u64,
        after: Option<ConversationListCursor>,
    ) -> Result<Self, ConversationListQueryError>;
    pub fn try_new_with_page_limits(
        title_contains: Option<String>,
        origin: ConversationOriginFilter,
        include_archived: bool,
        page_size: u64,
        after: Option<ConversationListCursor>,
        minimum_page_size: Option<u64>,
        maximum_page_size: Option<u64>,
    ) -> Result<Self, ConversationListQueryError>;
    // accessors: title_contains(), origin(), include_archived(), page_size(),
    // after()
}

pub enum ConversationListQueryError {
    EmptyTitleSearch,
    TitleSearchContainsNul,
    TitleSearchExceedsUtf8Bytes,
    PageSizeOutOfRange,
}

pub enum ConversationListItem {
    NativeSession {
        session: SessionId,
        title: Option<String>,
        archived: bool,
        defaults_version: SessionConfigurationDefaultsVersion,
    },
    ImportedConversation {
        conversation: ImportedConversationId,
        title: Option<String>,
        entry_count: u64,
        format: ImportedConversationFormat,
    },
}
impl ConversationListItem {
    pub const fn cursor(&self) -> ConversationListCursor;
    pub fn title(&self) -> Option<&str>;
}

pub trait ConversationPageReader {
    type Error;
    fn next_item(
        &mut self,
    ) -> impl Future<Output = Result<Option<ConversationListItem>, Self::Error>> + Send;
    fn next_after(&self) -> Option<ConversationListCursor>;
}

pub trait ConversationLister {
    type Error;
    type Page: ConversationPageReader<Error = Self::Error>;
    fn open_conversation_page(
        &self,
        query: ConversationListQuery,
    ) -> impl Future<Output = Result<Self::Page, Self::Error>> + Send;
}

pub struct ListConversationsService<Lister> { /* private */ }
impl<Lister> ListConversationsService<Lister> {
    pub const fn new(lister: Lister) -> Self;
    pub fn into_lister(self) -> Lister;
}
impl<Lister: ConversationLister> ListConversationsService<Lister> {
    pub async fn execute(
        &self,
        query: ConversationListQuery,
    ) -> Result<Lister::Page, Lister::Error>;
}
```

## application: load_session

```rust
pub trait SessionReader {
    type Error;

    fn load_session(
        &self,
        session_id: SessionId,
    ) -> impl Future<Output = Result<Option<Session>, Self::Error>> + Send;
}

pub struct LoadSessionService<Reader> { /* private */ }
impl<Reader> LoadSessionService<Reader> {
    pub const fn new(reader: Reader) -> Self;
    pub fn into_reader(self) -> Reader;
}
impl<Reader: SessionReader> LoadSessionService<Reader> {
    pub async fn execute(&self, session_id: SessionId)
        -> Result<Option<Session>, Reader::Error>;
}
```

## application: search

```rust
pub const MAX_SEARCH_HIGHLIGHTS_PER_RESULT: usize;

pub const fn max_search_query_bytes() -> usize;
pub const fn max_search_page_items() -> u16;
pub const fn max_search_snippet_bytes() -> usize;
pub const fn max_search_highlights_per_result() -> usize;
pub const fn max_search_projection_text_bytes() -> usize;

pub enum SearchTextError { Empty, TooLong, ContainsNul }

pub struct SearchText(/* private String */);
impl SearchText {
    pub fn try_new(value: String) -> Result<Self, SearchTextError>;
    pub fn as_str(&self) -> &str;
}

pub enum SearchStrategy { Lexical }

pub enum SearchScope { Global, Session(SessionId) }

pub struct SearchPageLimitError;

pub struct SearchPageLimit(/* private u16 */);
impl SearchPageLimit {
    pub const fn new(value: u16) -> Result<Self, SearchPageLimitError>;
    pub const fn get(self) -> u16;
}

pub struct SearchCursor { /* private */ }
impl SearchCursor {
    pub const fn new(address: TimelineAddress, projection: NonZeroU64) -> Self;
    pub const fn address(self) -> TimelineAddress;
    pub const fn projection(self) -> NonZeroU64;
}

pub struct SearchQuery {
    pub strategy: SearchStrategy,
    pub scope: SearchScope,
    pub text: SearchText,
    pub limit: SearchPageLimit,
    pub after: Option<SearchCursor>,
}

pub enum SearchContentClass {
    UserTranscript,
    AssistantTranscript,
    ToolArguments,
    ToolResult,
    SessionMetadata,
    AttachmentFilename,
    AttachmentMediaMetadata,
    DerivedTextArtifact,
}

pub struct SearchArtifactId(/* private Uuid */);
impl SearchArtifactId {
    pub const fn from_uuid(value: Uuid) -> Self;
    pub const fn into_uuid(self) -> Uuid;
}

pub enum SearchProjectionTextError { Empty, TooLong, ContainsNul }

pub struct SearchProjectionText(/* private String */);
impl SearchProjectionText {
    pub fn try_new(value: String) -> Result<Self, SearchProjectionTextError>;
    pub fn as_str(&self) -> &str;
}

pub enum SearchArtifactProjectionClass {
    AttachmentFilename,
    AttachmentMediaMetadata,
    DerivedText,
}

pub struct SearchArtifactProjection {
    pub session: SessionId,
    pub address: TimelineAddress,
    pub artifact: SearchArtifactId,
    pub class: SearchArtifactProjectionClass,
    pub text: SearchProjectionText,
}

pub enum SearchResultSource {
    Session(SessionId),
    AcceptedInput { input: AcceptedInputId, turn: TurnId },
    SteeringInput { input: AcceptedInputId, source_turn: TurnId },
    TurnTranscriptEntry { entry: SemanticTranscriptEntryId, turn: TurnId },
    SessionTranscriptEntry { entry: SemanticTranscriptEntryId },
    ToolRequest { request: ToolRequestId, turn: TurnId },
    ToolAttempt { attempt: ToolAttemptId, turn: TurnId },
    Attachment { attachment: SearchArtifactId },
    DerivedArtifact { artifact: SearchArtifactId },
}

pub struct SearchHighlight { pub start_byte: u16, pub end_byte: u16 }
pub struct SearchResult {
    pub session: SessionId,
    pub address: TimelineAddress,
    pub projection: NonZeroU64,
    pub source: SearchResultSource,
    pub content_class: SearchContentClass,
    pub snippet: String,
    pub highlights: Vec<SearchHighlight>,
}
pub struct SearchPage {
    pub results: Vec<SearchResult>,
    pub next: Option<SearchCursor>,
}

pub trait SearchReader {
    type Error;
    fn search(&self, query: SearchQuery)
        -> impl Future<Output = Result<SearchPage, Self::Error>> + Send;
}

pub trait SearchProjectionWriter {
    type Error;
    fn publish(&self, projection: SearchArtifactProjection)
        -> impl Future<Output = Result<(), Self::Error>> + Send;
}

pub struct SearchService<Reader> { /* private */ }
impl<Reader> SearchService<Reader> {
    pub const fn new(reader: Reader) -> Self;
}
impl<Reader: SearchReader> SearchService<Reader> {
    pub async fn search(&self, query: SearchQuery) -> Result<SearchPage, Reader::Error>;
}
```

## application: usage

```rust
pub const fn max_usage_call_page_items() -> u16;
pub const fn max_usage_aggregate_groups() -> u16;
pub const fn max_usage_aggregate_calls() -> u16;
pub const fn max_usage_credential_profile_utf8_bytes() -> u16;

pub struct UsageTimestampError {
    pub rejected_micros: u64,
}

pub struct UsageTimestampMicros(/* private u64 */);
impl UsageTimestampMicros {
    pub const fn new(value: u64) -> Result<Self, UsageTimestampError>;
    pub const fn get(self) -> u64;
}

pub struct UsageTimeRangeError {
    pub from_inclusive_micros: u64,
    pub to_exclusive_micros: u64,
}

pub struct UsageTimeFromInclusive(pub UsageTimestampMicros);
pub struct UsageTimeToExclusive(pub UsageTimestampMicros);

pub struct UsageTimeRange { /* private */ }
impl UsageTimeRange {
    pub const fn all() -> Self;
    pub const fn new(
        from_inclusive: Option<UsageTimeFromInclusive>,
        to_exclusive: Option<UsageTimeToExclusive>,
    ) -> Result<Self, UsageTimeRangeError>;
    pub const fn from_inclusive(self) -> Option<UsageTimestampMicros>;
    pub const fn to_exclusive(self) -> Option<UsageTimestampMicros>;
}

pub enum UsageCallKind { ModelCall, ApprovalJudge, ContextCompaction }

pub enum UsageCallScope {
    ModelCall(TurnId),
    ApprovalJudge(TurnId),
    ContextCompaction,
}
impl UsageCallScope {
    pub const fn call_kind(self) -> UsageCallKind;
    pub const fn turn(self) -> Option<TurnId>;
}

pub enum UsageProvenance { Reported, Estimated }
pub enum UsageInputTokenSemantics { Unknown, CacheExclusive, CacheInclusive }
pub enum UsageTokenPresence { Absent, Present }

pub struct UsageTokenCoverage {
    pub input: UsageTokenPresence,
    pub output: UsageTokenPresence,
    pub cache_creation_input: UsageTokenPresence,
    pub cache_read_input: UsageTokenPresence,
}

pub struct UsageTokenAxes {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_creation_input: Option<u64>,
    pub cache_read_input: Option<u64>,
}
impl UsageTokenAxes {
    pub const fn coverage(self) -> UsageTokenCoverage;
}

pub struct UsageAggregateTokenAxes {
    pub input: Option<u128>,
    pub output: Option<u128>,
    pub cache_creation_input: Option<u128>,
    pub cache_read_input: Option<u128>,
}

pub struct UsageSelection {
    pub session: Option<SessionId>,
    pub turn: Option<TurnId>,
    pub model: Option<ResolvedProviderTarget>,
    pub provenance: Option<UsageProvenance>,
    pub call_kind: Option<UsageCallKind>,
}
impl UsageSelection {
    pub const fn all() -> Self;
}

pub struct UsageQuery {
    pub time: UsageTimeRange,
    pub selection: UsageSelection,
}

pub struct UsageCallPageLimitError {
    pub rejected_items: u16,
}

pub struct UsageCallPageLimit(/* private u16 */);
impl UsageCallPageLimit {
    pub const fn new(value: u16) -> Result<Self, UsageCallPageLimitError>;
    pub const fn get(self) -> u16;
}

pub enum UsageCallOrder { NewestFirst }

pub struct UsageCallCursor {
    pub recorded_at: UsageTimestampMicros,
    pub call: ModelCallId,
}

pub struct UsageCallQuery {
    pub scope: UsageQuery,
    pub order: UsageCallOrder,
    pub limit: UsageCallPageLimit,
    pub after: Option<UsageCallCursor>,
}

pub enum UsageCredentialProfileLabelError {
    Empty,
    Oversized { rejected_utf8_bytes: usize },
    UndiscriminatedForm,
}

pub struct UsageCredentialProfileLabel(/* private String */);
impl UsageCredentialProfileLabel {
    pub fn new(label: String) -> Result<Self, UsageCredentialProfileLabelError>;
    pub fn as_str(&self) -> &str;
    pub fn into_string(self) -> String;
}

pub struct UsageCallEvidence {
    pub scope: UsageCallScope,
    pub call: ModelCallId,
    pub session: SessionId,
    pub model: ResolvedProviderTarget,
    pub credential_profile: UsageCredentialProfileLabel,
    pub credential_reference: Option<String>,
    pub provenance: UsageProvenance,
    pub input_semantics: UsageInputTokenSemantics,
    pub tokens: UsageTokenAxes,
    pub recorded_at: UsageTimestampMicros,
}

pub enum UsageCallPageContinuation { Exhausted, HasMore }

pub enum UsageCallPageError {
    Overflow { returned_calls: usize, limit_items: u16 },
    DanglingContinuation,
    Misordered { position: usize },
}

pub struct UsageCallPage { /* private */ }
impl UsageCallPage {
    pub fn new(
        calls: Vec<UsageCallEvidence>,
        continuation: UsageCallPageContinuation,
        limit: UsageCallPageLimit,
    ) -> Result<Self, UsageCallPageError>;
    pub fn calls(&self) -> &[UsageCallEvidence];
    pub const fn next(&self) -> Option<UsageCallCursor>;
}

pub struct UsageAggregateKey {
    pub call_kind: UsageCallKind,
    pub model: ResolvedProviderTarget,
    pub credential_profile: UsageCredentialProfileLabel,
    pub credential_reference: Option<String>,
    pub provenance: UsageProvenance,
    pub input_semantics: UsageInputTokenSemantics,
    pub coverage: UsageTokenCoverage,
}

pub enum UsageCacheNormalization { Unsafe, Safe }
pub enum UsageAggregateCompleteness { Complete, Truncated }

pub enum UsageTokenAxis { Input, Output, CacheCreationInput, CacheReadInput }

pub enum UsageAggregateGroupError {
    Coverage { axis: UsageTokenAxis, declared: UsageTokenPresence },
    NormalizationClaim {
        claimed: UsageCacheNormalization,
        input_semantics: UsageInputTokenSemantics,
    },
}

pub struct UsageAggregateGroup { /* private */ }
impl UsageAggregateGroup {
    pub fn new(
        key: UsageAggregateKey,
        call_count: u64,
        tokens: UsageAggregateTokenAxes,
        cache_normalization: UsageCacheNormalization,
    ) -> Result<Self, UsageAggregateGroupError>;
    pub const fn key(&self) -> &UsageAggregateKey;
    pub const fn call_count(&self) -> u64;
    pub const fn tokens(&self) -> UsageAggregateTokenAxes;
    pub const fn cache_normalization(&self) -> UsageCacheNormalization;
}

pub enum UsageAggregateReportError {
    GroupOverflow { returned_groups: usize },
    SourceCallOverflow { represented_calls: u128 },
}

pub struct UsageAggregateReport { /* private */ }
impl UsageAggregateReport {
    pub fn new(
        groups: Vec<UsageAggregateGroup>,
        completeness: UsageAggregateCompleteness,
    ) -> Result<Self, UsageAggregateReportError>;
    pub fn groups(&self) -> &[UsageAggregateGroup];
    pub const fn completeness(&self) -> UsageAggregateCompleteness;
}

pub trait UsageReader {
    type Error;
    fn aggregate(&self, query: UsageQuery)
        -> impl Future<Output = Result<UsageAggregateReport, Self::Error>> + Send;
    fn calls(&self, query: UsageCallQuery)
        -> impl Future<Output = Result<UsageCallPage, Self::Error>> + Send;
}

pub struct UsageService<Reader> { /* private */ }
impl<Reader> UsageService<Reader> {
    pub const fn new(reader: Reader) -> Self;
}
impl<Reader: UsageReader> UsageService<Reader> {
    pub async fn aggregate(
        &self,
        query: UsageQuery,
    ) -> Result<UsageAggregateReport, Reader::Error>;
    pub async fn calls(
        &self,
        query: UsageCallQuery,
    ) -> Result<UsageCallPage, Reader::Error>;
}
```

## application: session_timeline

```rust
pub const fn max_timeline_window_items() -> u16;
pub const fn max_timeline_window_bytes() -> u32;
pub const fn min_timeline_window_bytes() -> u32;
pub const fn max_timeline_detail_items() -> u16;
pub const fn max_timeline_detail_bytes() -> u32;
pub const fn min_timeline_detail_bytes() -> u32;
pub const fn timeline_detail_envelope_bytes() -> u32;

pub struct TimelineAddress(/* private NonZeroU64 */);
impl TimelineAddress {
    pub const fn new(sequence: NonZeroU64) -> Self;
    pub const fn sequence(self) -> NonZeroU64;
}

pub enum TimelineWindowAnchor {
    First,
    Latest,
    Before(TimelineAddress),
    After(TimelineAddress),
    Around(TimelineAddress),
}

pub enum TimelineWindowLimitError { Items, Bytes }

pub struct TimelineWindowLimits { /* private */ }
impl TimelineWindowLimits {
    pub const fn new(max_items: u16, max_projected_bytes: u32)
        -> Result<Self, TimelineWindowLimitError>;
    pub const fn max_items(self) -> u16;
    pub const fn max_projected_bytes(self) -> u32;
}

pub enum SessionTimelineEventKind {
    SessionCreated,
    SessionStateChanged,
    SessionTerminal,
    GoalChanged,
    CommandSettled,
    InjectionSettled,
    SessionOwnershipChanged,
    SessionModelSettingsChanged,
    TurnModelSettingsResolved,
    InputAccepted,
    GoalTurnRetired,
    TurnActivated,
    TurnFailed,
    ModelCallTransition,
    ToolBatchTransition,
    ToolApprovalDecided,
    ContextCompacted,
    TurnCompleted,
    TurnRefused,
    TurnCancelled,
    TurnReconciliationRequired,
    RunnerStateTransition,
    DelegationUpdate,
    DelegationWake,
}

pub struct SessionTimelineItem {
    pub address: TimelineAddress,
    pub kind: SessionTimelineEventKind,
    pub projected_structured_bytes: u32,
}
pub struct SessionTimelineBounds {
    pub first: Option<TimelineAddress>,
    pub latest: Option<TimelineAddress>,
}
pub struct SessionTimelineSizeFacts {
    pub item_count: u64,
    pub projected_text_bytes: u64,
    pub projected_structured_bytes: u64,
    pub referenced_blob_count: u64,
    pub referenced_blob_bytes: u64,
}
pub struct SessionWorkFacts {
    pub active_turn_count: u64,
    pub queued_turn_count: u64,
}
pub struct SessionTimelineDescriptor {
    pub session: SessionId,
    pub sizes: SessionTimelineSizeFacts,
    pub bounds: SessionTimelineBounds,
    pub work: SessionWorkFacts,
    pub observed_through: u64,
}
pub enum TimelineContinuation {
    Exhausted,
    MoreAt(TimelineAddress),
}
pub struct SessionTimelineWindow {
    pub session: SessionId,
    pub items: Vec<SessionTimelineItem>,
    pub projected_structured_bytes: u32,
    pub continuation_before: TimelineContinuation,
    pub continuation_after: TimelineContinuation,
}

pub enum TimelineDetailLimitError { Items, Bytes }
pub struct TimelineDetailLimits { /* private */ }
impl TimelineDetailLimits {
    pub const fn new(max_items: u16, max_projected_bytes: u32)
        -> Result<Self, TimelineDetailLimitError>;
    pub const fn max_items(self) -> u16;
    pub const fn max_projected_bytes(self) -> u32;
}
pub enum TimelineBodyField {
    InputText,
    ModelResponse,
}
pub struct TimelineBodyContinuation {
    pub address: TimelineAddress,
    pub field: TimelineBodyField,
    pub member_index: u32,
    pub offset_bytes: u64,
}
pub struct TimelineDetailCursor {
    pub address: TimelineAddress,
    pub field: Option<TimelineBodyField>,
    pub member_index: u32,
    pub offset_bytes: u64,
}
pub enum TimelineDetailContinuation {
    MoreAt(TimelineAddress),
    MoreBody(TimelineBodyContinuation),
}
pub struct TimelineTextExcerpt {
    pub text: String,
    pub offset_bytes: u64,
    pub total_bytes: u64,
    pub continuation: Option<TimelineBodyContinuation>,
}
pub struct TimelineBlobReference {
    pub blob_id: BlobDigest,
    pub length_bytes: u64,
    pub media_type: Option<String>,
}
pub enum TimelineModelCallState {
    Prepared,
    InFlight,
    CancellationRequested,
    Terminal(TimelineModelCallDisposition),
}
pub enum TimelineModelCallDisposition {
    Completed,
    KnownFailed,
    Refused,
    Cancelled,
    Ambiguous,
}
pub struct TimelineModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}
pub enum TimelineTurnLifecycleKind { Activated, Terminalized }
pub enum SessionTimelineDetailBody {
    UserInput {
        turn_id: TurnId,
        text: TimelineTextExcerpt,
        attachments: Vec<TimelineBlobReference>,
    },
    ModelCall {
        turn_id: TurnId,
        model_call_id: ModelCallId,
        state: TimelineModelCallState,
        model_identity_id: ProviderModelIdentity,
        request_context_items: u64,
        response: Option<TimelineTextExcerpt>,
        usage: TimelineModelUsage,
        provider_failure_cause: Option<ProviderModelCallFailureCause>,
    },
    TurnLifecycle {
        turn_id: TurnId,
        lifecycle: TimelineTurnLifecycleKind,
        cause_code: String,
    },
    EventFact { kind: SessionTimelineEventKind },
}
pub struct SessionTimelineDetail {
    pub address: TimelineAddress,
    pub kind: SessionTimelineEventKind,
    pub body: SessionTimelineDetailBody,
    pub projected_body_bytes: u32,
}
pub struct SessionTimelineDetailPage {
    pub session: SessionId,
    pub items: Vec<SessionTimelineDetail>,
    pub projected_body_bytes: u32,
    pub continuation: Option<TimelineDetailContinuation>,
}

pub trait SessionTimelineReader {
    type Error;
    fn read_descriptor(&self, session: SessionId)
        -> impl Future<Output = Result<Option<SessionTimelineDescriptor>, Self::Error>> + Send;
    fn read_window(
        &self,
        session: SessionId,
        anchor: TimelineWindowAnchor,
        limits: TimelineWindowLimits,
    ) -> impl Future<Output = Result<Option<SessionTimelineWindow>, Self::Error>> + Send;
    fn read_item_details(
        &self,
        session: SessionId,
        address: TimelineAddress,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> impl Future<Output = Result<Option<SessionTimelineDetailPage>, Self::Error>> + Send;
    fn read_turn_details(
        &self,
        session: SessionId,
        turn: TurnId,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> impl Future<Output = Result<Option<SessionTimelineDetailPage>, Self::Error>> + Send;
    fn read_region_details(
        &self,
        session: SessionId,
        first: TimelineAddress,
        through: TimelineAddress,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> impl Future<Output = Result<Option<SessionTimelineDetailPage>, Self::Error>> + Send;
}

pub struct ReadSessionTimelineService<Reader> { /* private */ }
impl<Reader> ReadSessionTimelineService<Reader> {
    pub const fn new(reader: Reader) -> Self;
}
impl<Reader: SessionTimelineReader> ReadSessionTimelineService<Reader> {
    pub async fn descriptor(&self, session: SessionId)
        -> Result<Option<SessionTimelineDescriptor>, Reader::Error>;
    pub async fn window(
        &self,
        session: SessionId,
        anchor: TimelineWindowAnchor,
        limits: TimelineWindowLimits,
    ) -> Result<Option<SessionTimelineWindow>, Reader::Error>;
    pub async fn item_details(
        &self,
        session: SessionId,
        address: TimelineAddress,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> Result<Option<SessionTimelineDetailPage>, Reader::Error>;
    pub async fn turn_details(
        &self,
        session: SessionId,
        turn: TurnId,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> Result<Option<SessionTimelineDetailPage>, Reader::Error>;
    pub async fn region_details(
        &self,
        session: SessionId,
        first: TimelineAddress,
        through: TimelineAddress,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> Result<Option<SessionTimelineDetailPage>, Reader::Error>;
}
```

## application: session_live

```rust
pub const fn max_session_live_queued_turns() -> u8;

pub enum SessionLiveActiveState {
    Running { model_call: Option<ModelCallId> },
    AwaitingModelCallRecovery { call: ModelCallId },
    AwaitingToolApproval { request: ToolRequestId },
    AwaitingChild { request: ToolRequestId, child: SessionId },
    AwaitingToolRecovery { attempt: ToolAttemptId },
    AwaitingRunnerRecovery { runner: RunnerId, placement_revision: u64 },
}
pub struct SessionLiveActiveTurn {
    pub turn: TurnId,
    pub state: SessionLiveActiveState,
}
pub enum SessionLiveReconciliation {
    ModelCall { turn: TurnId, call: ModelCallId },
    ToolAttempt { turn: TurnId, attempt: ToolAttemptId },
}
pub enum SessionLiveRunnerState {
    Unpinned,
    Pinned,
    RunnerLostBeforePin,
    RunnerLost,
    RunnerAbandoned,
}
pub enum SessionLiveRunnerConnectionHealth { Connected, Suspect, Shutdown, Lost }
pub struct SessionLiveRunner {
    pub runner: Option<RunnerId>,
    pub placement_revision: u64,
    pub state: SessionLiveRunnerState,
    pub connection_health: Option<SessionLiveRunnerConnectionHealth>,
}
pub struct SessionLiveSnapshot {
    pub session: SessionId,
    pub observed_through: u64,
    pub active: Option<SessionLiveActiveTurn>,
    pub queued_turn_count: u64,
    pub queued_turns: Vec<TurnId>,
    pub reconciliation: Option<SessionLiveReconciliation>,
    pub runner: Option<SessionLiveRunner>,
}
pub trait SessionLiveReader {
    type Error;
    fn read_live_snapshot(&self, session: SessionId)
        -> impl Future<Output = Result<Option<SessionLiveSnapshot>, Self::Error>> + Send;
}
pub struct ReadSessionLiveService<Reader> { /* private */ }
impl<Reader> ReadSessionLiveService<Reader> {
    pub const fn new(reader: Reader) -> Self;
}
impl<Reader: SessionLiveReader> ReadSessionLiveService<Reader> {
    pub async fn snapshot(&self, session: SessionId)
        -> Result<Option<SessionLiveSnapshot>, Reader::Error>;
}
```

## application: model_execution

```rust
pub struct ModelCallCredentialReference { /* private */ }
impl ModelCallCredentialReference {
    pub fn new(value: impl Into<String>) -> Self;
    pub fn as_str(&self) -> &str;
}

pub enum ModelUserContentPart {
    Text(NonEmptyUnicodeText),
    AttachmentStub(ModelAttachmentStub),
}
impl ModelUserContentPart {
    pub fn as_str(&self) -> &str;
}

pub struct ModelUserContent { /* private */ }
impl ModelUserContent {
    // accessors: parts(), single_text()
}

pub struct ModelAttachmentStub { /* private */ }
impl ModelAttachmentStub {
    pub fn as_str(&self) -> &str;
}
// Debug is content-redacted.

pub enum ModelConversationMessage {
    ModelIdentityChanged {
        source: SemanticTranscriptEntryRef,
        defaults_version: SessionConfigurationDefaultsVersion,
        selected: DirectModelSelection,
    },
    ContextSummary {
        source: SemanticTranscriptEntryRef,
        producing_call: ModelCallId,
        summarized: ContextCompactionRange,
        content: AssistantText,
    },
    User {
        source: SemanticTranscriptEntryRef,
        accepted_input: AcceptedInputId,
        content: ModelUserContent,
    },
    DelegatedTask {
        source: SemanticTranscriptEntryRef,
        spawning_request: ToolRequestId,
        parent_session: SessionId,
        parent_turn: TurnId,
        content: DelegationContent,
    },
    DelegationMessage {
        source: SemanticTranscriptEntryRef,
        spawning_request: ToolRequestId,
        message: DelegationMessageId,
        sender: SessionId,
        recipient: SessionId,
        delivery_sequence: NonZeroU64,
        content: DelegationContent,
    },
    BackgroundDelegationResult {
        source: SemanticTranscriptEntryRef,
        awaiting_request: ToolRequestId,
        spawning_request: ToolRequestId,
        child: SessionId,
        delivery_sequence: NonZeroU64,
        outcome: DelegationOutcome,
    },
    Assistant {
        source: SemanticTranscriptEntryRef,
        producing_call: ModelCallId,
        content: AssistantText,
    },
    AssistantToolUse {
        source: SemanticTranscriptEntryRef,
        producing_call: ModelCallId,
        request: ToolRequest,
    },
    ToolResult {
        source: SemanticTranscriptEntryRef,
        request: ToolRequestId,
        content: ModelToolResultContent,
    },
    ImportedUser {
        source: SemanticTranscriptEntryRef,
        imported_entry: ImportedTranscriptEntryId,
        content: ImportedText,
    },
    ImportedAssistant {
        source: SemanticTranscriptEntryRef,
        imported_entry: ImportedTranscriptEntryId,
        content: ImportedText,
    },
}

pub enum ModelToolResultContent {
    Success(ToolResultContent),
    ExecutionError(ToolExecutionError),
    Denied { reason: Option<ToolDenialReason> },
    ClosedByTurnEnd,
    Delegation(DelegationOutcome),
}

pub struct PreparedModelOperation { /* private */ }
impl PreparedModelOperation {
    pub fn render(
        request: PreparedModelCallRequest,
        credential_reference: ModelCallCredentialReference,
        system_prompt: Option<SessionSystemPrompt>,
        tools: Box<[ToolDefinition]>,
        tool_entries: &[ResolvedToolConversationEntry],
    ) -> Result<Self, ModelFrontierRenderingError>;
    // accessors: request(), credential_reference(), system_prompt(), messages(), tools(),
    // attachment_digests()
}

pub fn render_model_user_content(
    content: UserContent,
    attachment_byte_length: impl FnMut(BlobDigest) -> Option<NonZeroU64>,
) -> Result<ModelUserContent, ModelFrontierRenderingError>;

pub enum ModelFrontierRenderingError {
    MissingOriginContent {
        entry: SemanticTranscriptEntryRef,
        accepted_input: AcceptedInputId,
    },
    MissingAttachmentBlobFact { digest: BlobDigest },
    AttachmentStubSerialization,
    AttachmentStubBoundExceeded,
    DuplicateToolEvidence { entry: SemanticTranscriptEntryRef },
    MissingOrMismatchedToolEvidence { entry: SemanticTranscriptEntryRef },
    UnrenderableToolResult { entry: SemanticTranscriptEntryRef },
    UnexpectedToolEvidence { entry: SemanticTranscriptEntryRef },
    MissingProjectedEntry { entry: SemanticTranscriptEntryRef },
    InvalidDelegationDelivery { entry: SemanticTranscriptEntryRef },
    InvalidContextProjection(ContextFrontierProjectionFailure),
}
// impl Display + std::error::Error + ClassifyOperatorFailure

pub enum PrepareModelCallOutcome {
    NoWork,
    RetryBackoff(std::time::Duration),
    PoolExhausted(Box<CredentialPoolExhaustedModelCallTurn>),
    Checkpointed(ModelCallId),
    Ready {
        request: Box<PreparedModelCallRequest>,
        credential_reference: ModelCallCredentialReference,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
        recorded_user_overrides: Box<[RecordedUserOverride]>,
        system_prompt: Option<SessionSystemPrompt>,
        tool_entries: Box<[ResolvedToolConversationEntry]>,
    },
    TargetUnavailable(Box<FailedModelCallTurn>),
}

pub trait PrepareModelCallTransaction {
    type Error: ClassifyOperatorFailure;
    fn prepare<NextSteeringIdentities>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        failure_identities: FailedModelCallTurnIdentities,
        steering_frontier: ContextFrontierId,
        next_steering_identities: NextSteeringIdentities,
    ) -> impl Future<Output = Result<PrepareModelCallOutcome, Self::Error>> + Send
    where
        NextSteeringIdentities:
            FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send;
}

pub trait FailPreparedModelCallTransaction {
    type Error: ClassifyOperatorFailure;
    fn fail_prepared<NextTurn>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        cause: PreparedModelCallFailureCause,
        attachment_failure: Option<AttachmentPreparationFailure>,
        identities: FailedModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<FailedModelCallTurn, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;
    fn reread_failure(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        attachment_failure: Option<AttachmentPreparationFailure>,
    ) -> impl Future<Output = Result<RetainedPreparedFailureStatus, Self::Error>> + Send;
}

pub enum PreparedModelCallFailureCause {
    CapabilityKnownFailure,
    ToolRoundLimitReached,
}

pub enum RetainedPreparedFailureStatus {
    Pending,
    AlreadyCommitted,
    Cancelled,
}

pub trait AuthorizeModelCallTransaction {
    type Error: ClassifyOperatorFailure;
    fn authorize(
        &mut self,
        session: SessionId,
        call: ModelCallId,
    ) -> impl Future<Output = Result<AuthorizeModelCallOutcome, Self::Error>> + Send;
    fn reread_after_ambiguous_commit(
        &mut self,
        session: SessionId,
        prepared: &PreparedModelCallRequest,
    ) -> impl Future<Output = Result<ModelCallAuthorizationReread, Self::Error>> + Send;
    fn cancellation_signal(
        &self,
        session: SessionId,
        call: ModelCallId,
    ) -> impl Future<Output = ()> + Send + 'static;
}

pub enum AuthorizeModelCallOutcome {
    NoSend,
    Authorized(Box<AuthorizedModelCall>),
}

pub enum ModelCallAuthorizationReread {
    Prepared,
    InFlight(Box<AuthorizedModelCall>),
    CancellationRequested(Box<StopRequestedModelCallTurn>),
    Cancelled,
}

pub enum ModelCallTerminalIdentityCandidates {
    Exact(ModelCallTerminalIdentities),
    Availability {
        failed: FailedModelCallTurnIdentities,
        successor_attempt: TurnAttemptId,
    },
    ToolRound {
        continuing: ToolRoundModelCallIdentities,
        stopped: StoppedToolRoundModelCallIdentities,
    },
}

pub enum ModelCallObservationCommitOutcome {
    Terminal(Box<ModelCallTerminalOutcome>),
    AvailabilitySuccessor(Box<AvailabilitySuccessorOutcome>),
    PoolExhausted(CredentialPoolExhaustedOutcome),
}

pub enum CredentialPoolExhaustedOutcome {
    BeforeCall(Box<CredentialPoolExhaustedModelCallTurn>),
    AfterCall {
        pool_name: Arc<str>,
        terminal: Box<ModelCallTerminalOutcome>,
    },
}

pub struct AvailabilitySuccessorOutcome { /* private */ }
impl AvailabilitySuccessorOutcome {
    pub const fn new(
        successor: AvailabilitySuccessorModelCallTurn,
        backoff: std::time::Duration,
    ) -> Self;
    // accessors: successor(), backoff()
}

pub trait CommitModelCallObservationTransaction {
    type Error: ClassifyOperatorFailure;
    fn commit_observation<NextTurn>(
        &mut self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentityCandidates,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<Option<ModelCallObservationCommitOutcome>, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;
    fn reread_observation(
        &mut self,
        session: SessionId,
        observation: &CorrelatedModelCallTerminalObservation,
    ) -> impl Future<Output = Result<RetainedModelCallObservationStatus, Self::Error>> + Send;
}

pub enum RetainedModelCallObservationStatus {
    Pending,
    AlreadyCommitted,
    DiscardedByLogicalTerminal,
}

pub struct RetainedModelCallExecutionState { /* private */ }

pub enum AttachmentPreparationFailure {
    TooLarge { maximum_bytes: u64 },
    Missing,
    Corrupt,
    Unavailable,
}

pub enum ModelCallCapabilityPreparation<Capability> {
    Ready(Capability),
    Cancelled,
    KnownFailure,
    AttachmentFailure(AttachmentPreparationFailure),
}

pub enum ModelCallInputTokenCount {
    Counted(u64),
    Cancelled,
    AttachmentUnavailable,
    AttachmentFailure,
    Unavailable,
}

pub trait ModelCallInputTokenCounter {
    type Error: ClassifyOperatorFailure;
    fn count_input_tokens<Cancellation>(
        &self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<ModelCallInputTokenCount, Self::Error>> + Send
    where
        Cancellation: Future<Output = ()> + Send + 'static;
}

pub trait ModelCallProvider {
    type Capability;
    type Error: ClassifyOperatorFailure;
    fn prepare_capability<Cancellation>(
        &mut self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>>
           + Send
    where
        Cancellation: Future<Output = ()> + Send + 'static;
    fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        authorized: AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
        cancellation: Cancellation,
    ) -> impl Future<Output = Result<CorrelatedModelCallTerminalObservation, Self::Error>> + Send
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static;
}

pub trait ModelCallExecutionIdGenerator {
    fn next_model_call_id(&mut self) -> ModelCallId;
    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId;
    fn next_context_frontier_id(&mut self) -> ContextFrontierId;
    fn next_tool_request_id(&mut self) -> ToolRequestId;
    fn next_turn_attempt_id(&mut self) -> TurnAttemptId;
    fn next_turn_id(&mut self) -> TurnId;
}
pub struct UuidV7ModelCallExecutionIdGenerator;
// Default; impl ModelCallExecutionIdGenerator

pub trait AttemptDispatchGate {
    type Permit: Send;
    fn acquire(&self, attempt: TurnAttemptId) -> impl Future<Output = Self::Permit> + Send;
}
pub struct InProcessAttemptDispatchGate { /* private */ }
// Clone + Default; impl AttemptDispatchGate
pub struct InProcessAttemptDispatchPermit { /* private */ }

pub enum ModelCallExecutionOutcome {
    NoWork,
    RetryBackoff(std::time::Duration),
    PoolExhausted(Box<CredentialPoolExhaustedOutcome>),
    Checkpointed(ModelCallId),
    TargetUnavailable(Box<FailedModelCallTurn>),
    CapabilityKnownFailure(Box<FailedModelCallTurn>),
    AttachmentUnavailable,
    CapabilityFailureAlreadyCommitted(ModelCallId),
    ToolRoundLimitReached(Box<FailedModelCallTurn>),
    ToolRoundLimitAlreadyCommitted(ModelCallId),
    ObservationCommitted(Box<ModelCallTerminalOutcome>),
    AvailabilitySuccessor(Box<AvailabilitySuccessorOutcome>),
    ObservationAlreadyCommitted(ModelCallId),
}

pub enum ModelCallExecutionError<
    PrepareError,
    FailureError,
    AuthorizationError,
    ProviderError,
    ObservationError,
> {
    Prepare(PrepareError),
    Render(ModelFrontierRenderingError),
    CapabilityPreparation(ProviderError),
    PreparedFailureCommit(FailureError),
    PreparedFailureReread(FailureError),
    Authorization(AuthorizationError),
    AuthorizationReread {
        authorization_error: AuthorizationError,
        reread_error: AuthorizationError,
    },
    AuthorizationReconciliation(AuthorizationError),
    Provider(ProviderError),
    ObservationCommit {
        error: ObservationError,
        retained_observation: CorrelatedModelCallTerminalObservation,
    },
}
// impl Display + std::error::Error + ClassifyOperatorFailure (bounded)

pub struct ModelCallExecutionService<
    Ids,
    Prepare,
    Failure,
    Authorization,
    Observation,
    Provider,
    Gate,
> { /* private */ }
impl<Ids, Prepare, Failure, Authorization, Observation, Provider, Gate>
    ModelCallExecutionService<
        Ids,
        Prepare,
        Failure,
        Authorization,
        Observation,
        Provider,
        Gate,
    >
{
    pub fn new(
        ids: Ids,
        prepare: Prepare,
        failure: Failure,
        authorization: Authorization,
        observation: Observation,
        provider: Provider,
        gate: Gate,
        max_automatic_tool_rounds_per_turn: Option<usize>,
    ) -> Self;
    pub fn with_tool_catalog(self, catalog: impl ToolCatalog + 'static) -> Self;
    pub fn from_parts(
        ids: Ids,
        prepare: Prepare,
        failure: Failure,
        authorization: Authorization,
        observation: Observation,
        provider: Provider,
        gate: Gate,
        catalog: Arc<dyn ToolCatalog>,
        retained_state: Option<RetainedModelCallExecutionState>,
        max_automatic_tool_rounds_per_turn: Option<usize>,
    ) -> Self;
    pub fn into_parts(
        self,
    ) -> (
        Ids,
        Prepare,
        Failure,
        Authorization,
        Observation,
        Provider,
        Gate,
        Arc<dyn ToolCatalog>,
        Option<RetainedModelCallExecutionState>,
        Option<usize>,
    );
    pub const fn retained_state(&self) -> Option<&RetainedModelCallExecutionState>;
    pub fn retained_observation(&self) -> Option<&CorrelatedModelCallTerminalObservation>;
    pub async fn execute(
        &mut self,
        session: SessionId,
    ) -> Result<ModelCallExecutionOutcome, ModelCallExecutionError</* port errors */>>;
}

pub enum ScriptedModelCallStep {
    CapabilityKnownFailure,
    CapabilityOperatorFailure,
    InteractionOperatorFailure,
    Return(ModelCallTerminalObservation),
}
pub enum ScriptedModelCallError {
    ScriptExhausted,
    CapabilityOperatorFailure,
    InteractionOperatorFailure,
    AuthorizationMismatch,
}
// impl Display + std::error::Error + ClassifyOperatorFailure
pub struct ScriptedModelCallCapability { /* private */ }
pub struct ScriptedModelCallProvider { /* private */ }
impl ScriptedModelCallProvider {
    pub fn new(steps: impl IntoIterator<Item = ScriptedModelCallStep>) -> Self;
    // accessors: capability_preparation_count(), interaction_count(), remaining_step_count(),
    // last_prepared_messages(), last_prepared_tools(), last_prepared_system_prompt()
}
// impl ModelCallProvider
```

## application: tool_loop

```rust
pub struct ToolInputSchema { /* private */ }
impl ToolInputSchema {
    pub fn try_new(value: String) -> Result<Self, ToolInputSchemaError>;
    pub fn as_str(&self) -> &str;
}

pub enum ToolInputSchemaFailure {
    NotJson,
    NotObject,
    OutsideArgumentBound(ToolArgumentsFailure),
}

pub struct ToolInputSchemaError { /* private */ }
impl ToolInputSchemaError {
    pub fn value(&self) -> &str;
    pub const fn failure(&self) -> ToolInputSchemaFailure;
    pub fn into_parts(self) -> (String, ToolInputSchemaFailure);
}

pub struct ToolDefinition { /* private */ }
impl ToolDefinition {
    pub const fn new(
        name: ToolName,
        description: String,
        input_schema: ToolInputSchema,
        permission_default: ToolPermissionDefault,
        effect_class: ToolEffectClass,
    ) -> Self;
    pub const fn with_approval_posture(self, posture: ToolApprovalPosture) -> Self;
    pub const fn approval_posture(&self) -> Option<ToolApprovalPosture>;
    // accessors: name(), description(), input_schema(), permission_default(), effect_class()
}

pub trait ToolArgumentValidator: Send + Sync {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail>;
    fn preauthorization(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<ToolPreauthorization, ToolExecutionErrorDetail>;
}
// implemented for matching Fn(&NormalizedToolArguments) -> Result<(), ToolExecutionErrorDetail>

pub enum ToolPreauthorization {
    Unmetered,
    BlobMetadata { digest: BlobDigest },
    BlobRead {
        digest: BlobDigest,
        decoded_bytes: NonZeroU64,
    },
}

pub struct CompiledTool { /* private */ }
impl CompiledTool {
    pub fn new(
        definition: ToolDefinition,
        validator: impl ToolArgumentValidator + 'static,
    ) -> Self;
    pub const fn definition(&self) -> &ToolDefinition;
}

pub struct DuplicateToolDefinition { /* private */ }
impl DuplicateToolDefinition {
    pub const fn name(&self) -> &ToolName;
}

pub struct CompiledToolCatalog { /* private */ }
impl CompiledToolCatalog {
    pub fn try_new(
        tools: impl IntoIterator<Item = CompiledTool>,
    ) -> Result<Self, DuplicateToolDefinition>;
}
// Default; impl ToolCatalog

pub trait ToolCatalog: Send + Sync {
    fn definitions(&self) -> Box<[ToolDefinition]>;
    fn definition(&self, name: &ToolName) -> Option<ToolDefinition>;
    fn validate_arguments(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolCatalogValidationFailure>;
}

pub struct NoToolCatalog;
// Copy + Default; impl ToolCatalog

pub enum ToolCatalogValidationFailure {
    UnknownTool,
    InvalidArguments {
        detail: Option<ToolExecutionErrorDetail>,
    },
}

pub struct ToolExecutionInvocation { /* private */ }
// sealed: ToolExecutionService constructs only from checked request,
// declaration, and ToolDispatchAuthority.
impl ToolExecutionInvocation {
    // accessors: request(), dispatch_authority(), definition(), correlation()
    pub fn bind(self, evidence: ToolExecutorEvidence) -> CorrelatedToolExecutorEvidence;
    pub fn durable_completion(self) -> CorrelatedDurableToolCompletion;
}

pub enum ToolExecutorEvidence {
    CompletedText(String),
    KnownFailed {
        detail: Option<ToolExecutionErrorDetail>,
    },
    Ambiguous,
}

pub struct CorrelatedToolExecutorEvidence { /* private */ }
// sealed: ToolExecutionInvocation::bind
impl CorrelatedToolExecutorEvidence {
    // accessors: correlation(), evidence()
}

pub struct CorrelatedDurableToolCompletion { /* private */ }
// sealed: ToolExecutionInvocation::durable_completion
impl CorrelatedDurableToolCompletion {
    pub const fn correlation(self) -> ToolAttemptDispatchCorrelation;
}

pub struct CorrelatedDurableChildWait { /* private */ }
impl CorrelatedDurableChildWait {
    pub fn try_new(
        correlation: ToolAttemptDispatchCorrelation,
        wait: DelegationWait,
    ) -> Option<Self>;
    // accessors: correlation(), wait(), child_wait()
}

pub enum ToolExecutorDisposition {
    Completed(CorrelatedToolExecutorEvidence),
    DurableCompletion(CorrelatedDurableToolCompletion),
    DurableChildWait(CorrelatedDurableChildWait),
}
pub trait ToolExecutor {
    type Error: ClassifyOperatorFailure;
    fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> impl Future<Output = Result<CorrelatedToolExecutorEvidence, Self::Error>> + Send;
    fn execute_with_scheduling(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> impl Future<Output = Result<ToolExecutorDisposition, Self::Error>> + Send
    where
        Self: Send;
}

pub trait ToolApprovalIdGenerator {
    fn next_tool_turn_attempt_id(&mut self) -> TurnAttemptId;
}

pub trait ToolExecutionIdGenerator {
    fn next_tool_turn_attempt_id(&mut self) -> TurnAttemptId;
    fn next_tool_attempt_id(&mut self) -> ToolAttemptId;
    fn next_tool_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId;
    fn next_tool_context_frontier_id(&mut self) -> ContextFrontierId;
    fn next_tool_model_call_id(&mut self) -> ModelCallId;
    fn next_tool_turn_id(&mut self) -> TurnId;
}

pub struct UuidV7ToolLoopIdGenerator;
// Copy + Default; impl ToolApprovalIdGenerator + ToolExecutionIdGenerator

pub struct DecideToolRequestService<Ids, Transaction> { /* private */ }
impl<Ids, Transaction> DecideToolRequestService<Ids, Transaction> {
    pub const fn new(ids: Ids, transaction: Transaction) -> Self;
    pub fn into_parts(self) -> (Ids, Transaction);
}
impl<Ids: ToolApprovalIdGenerator + Send, Transaction: DecideToolRequestTransaction>
    DecideToolRequestService<Ids, Transaction>
{
    pub async fn execute(
        &mut self,
        command: DecideToolRequest,
    ) -> Result<PreparedDecideToolRequest, Transaction::Error>;
}

pub struct OverrideDeniedToolRequestService<Transaction> { /* private */ }
impl<Transaction> OverrideDeniedToolRequestService<Transaction> {
    pub const fn new(transaction: Transaction) -> Self;
    pub fn into_transaction(self) -> Transaction;
}
impl<Transaction: OverrideDeniedToolRequestTransaction>
    OverrideDeniedToolRequestService<Transaction>
{
    pub async fn execute(
        &mut self,
        command: OverrideDeniedToolRequest,
    ) -> Result<PreparedOverrideDeniedToolRequest, Transaction::Error>;
}

pub struct RetainedToolExecutionState { /* private */ }

pub enum ToolExecutionServiceOutcome {
    NoWork,
    AwaitingApproval(ToolRequestId),
    AwaitingRecovery(ToolAttemptId),
    ChildWaitResumed(TurnAttemptId),
    ChildWaitParked(ChildWait),
    AttemptCheckpointed(ToolAttemptId),
    PreflightFailed(Box<EndedToolAttempt>),
    ObservationCommitted(Box<EndedToolAttempt>),
    ObservationAlreadyCommitted(ToolAttemptId),
    CrashClassified(Box<ToolAttemptCrashOutcome>),
    ContinuationCheckpointed(ModelCallId),
    ContinuationTargetUnavailable(Box<FailedModelCallTurn>),
    ContinuationPoolExhausted(Box<CredentialPoolExhaustedModelCallTurn>),
    ContinuationContextCompactionRequired(Box<ContextHeadroomExhaustedModelCallTurn>),
}

pub enum ToolExecutionServiceError<TransactionError, ExecutorError> {
    Load(TransactionError),
    Prepare(TransactionError),
    Authorize(TransactionError),
    AuthorizationReread {
        authorization_error: TransactionError,
        reread_error: TransactionError,
    },
    AuthorizationReconciliation(TransactionError),
    PreflightCommit(TransactionError),
    Executor(ExecutorError),
    ExecutorCrashClassification {
        executor_error: ExecutorError,
        classification_error: TransactionError,
    },
    ExecutorCorrelationMismatch,
    ExecutorCorrelationMismatchCrashClassification(TransactionError),
    ObservationCommit(TransactionError),
    ObservationReconciliation(TransactionError),
    DurableCompletionReconciliation(TransactionError),
    DurableCompletionMismatch,
    ChildWaitReconciliation(TransactionError),
    ChildWaitMismatch,
    CrashClassification(TransactionError),
    RecoveredFatalExecutorFailure {
        failure_class: OperatorFailureClass,
        cause_code: &'static str,
    },
    Continuation(TransactionError),
    CatalogDrift,
}
// impl Display + std::error::Error + ClassifyOperatorFailure (bounded)

pub struct ToolExecutionService<Ids, Transaction, Catalog, Executor> { /* private */ }
impl<Ids, Transaction, Catalog, Executor>
    ToolExecutionService<Ids, Transaction, Catalog, Executor>
{
    pub const fn new(
        ids: Ids,
        transaction: Transaction,
        catalog: Catalog,
        executor: Executor,
        gate: InProcessToolDispatchGate,
    ) -> Self;
    pub const fn from_parts(
        ids: Ids,
        transaction: Transaction,
        catalog: Catalog,
        executor: Executor,
        gate: InProcessToolDispatchGate,
        retained_state: Option<RetainedToolExecutionState>,
    ) -> Self;
    pub fn into_parts(
        self,
    ) -> (
        Ids,
        Transaction,
        Catalog,
        Executor,
        InProcessToolDispatchGate,
        Option<RetainedToolExecutionState>,
    );
    pub const fn retained_state(&self) -> Option<&RetainedToolExecutionState>;
}
impl<
        Ids: ToolExecutionIdGenerator + Send,
        Transaction: ToolExecutionTransaction,
        Catalog: ToolCatalog,
        Executor: ToolExecutor,
    > ToolExecutionService<Ids, Transaction, Catalog, Executor>
{
    pub async fn execute(
        &mut self,
        session: SessionId,
        turn: TurnId,
    ) -> Result<
        ToolExecutionServiceOutcome,
        ToolExecutionServiceError<Transaction::Error, Executor::Error>,
    >;
}
```

## application: replace_session_defaults

```rust
pub struct ReplaceSessionDefaultsRequest { /* private */ }
impl ReplaceSessionDefaultsRequest {
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        expected_current_version: SessionConfigurationDefaultsVersion,
        replacement: SessionConfigurationDefaults,
        prompt_member: PromptMemberStatement,
    ) -> Result<Self, InvalidDurableCommandId>;
    pub fn try_new_with_model_settings(
        command_id: DurableCommandId,
        session: SessionId,
        expected_current_version: SessionConfigurationDefaultsVersion,
        replacement: SessionConfigurationDefaults,
        caller_model_settings: ModelSettingsOverlay,
        prompt_member: PromptMemberStatement,
    ) -> Result<Self, InvalidDurableCommandId>;
    pub fn try_new_with_model_settings_adjustments(
        command_id: DurableCommandId,
        session: SessionId,
        expected_current_version: SessionConfigurationDefaultsVersion,
        replacement: SessionConfigurationDefaults,
        caller_model_settings: ModelSettingsOverlay,
        model_settings_adjustments: Vec<ModelChangeAdjustment>,
        prompt_member: PromptMemberStatement,
    ) -> Result<Self, InvalidDurableCommandId>;
    // accessors: command_id(), session(), expected_current_version(), replacement(),
    // caller_model_settings(), model_settings_adjustments(), prompt_member()
}

pub enum PromptMemberStatement {
    Stated,
    Unstated,
}

pub enum ReplaceSessionDefaultsOutcome {
    Recorded(ReplaceSessionDefaultsResult),
    ConflictingReuse { command_id: DurableCommandId },
    PromptRequiresStatedMember,
}

pub trait ReplaceSessionDefaultsTransaction {
    type Error;

    fn handle(
        &mut self,
        command: ReplaceSessionDefaults,
        prompt_member: PromptMemberStatement,
    ) -> impl Future<Output = Result<ReplaceSessionDefaultsOutcome, Self::Error>> + Send;
}

pub struct ReplaceSessionDefaultsService<Transaction> { /* private */ }
impl<Transaction> ReplaceSessionDefaultsService<Transaction> {
    pub const fn new(transaction: Transaction) -> Self;
    pub fn into_transaction(self) -> Transaction;
}
impl<Transaction: ReplaceSessionDefaultsTransaction> ReplaceSessionDefaultsService<Transaction> {
    pub async fn execute(
        &mut self,
        request: ReplaceSessionDefaultsRequest,
    ) -> Result<ReplaceSessionDefaultsOutcome, Transaction::Error>;
}
```

## application: convergence_reconciliation

```rust
pub enum PullRequestCheckState {
    CheckRunInProgress,
    CheckRunCompleted {
        conclusion: Option<String>,
    },
    StatusContext {
        state: String,
    },
}

pub struct PullRequestCheck { /* private */ }
impl PullRequestCheck {
    pub fn new(name: String, state: PullRequestCheckState) -> Self;
    // accessors: name(), state(), is_non_gating(), is_green(), observed_state()
}

pub enum PullRequestDraftState {
    ReadyForReview,
    Draft,
}
impl PullRequestDraftState {
    pub const fn is_draft(self) -> bool;
}

pub struct PullRequestConvergenceFacts { /* private */ }
impl PullRequestConvergenceFacts {
    pub fn new(
        head_sha: CommitSha,
        checked_head_sha: Option<CommitSha>,
        draft: PullRequestDraftState,
        unresolved_review_threads: u64,
        mergeable_state: MergeableState,
        checks: Vec<PullRequestCheck>,
    ) -> Self;
    // accessors: head_sha(), checked_head_sha(), draft(),
    // unresolved_review_threads(), mergeable_state(), checks()
}

pub enum PullRequestConvergenceBlocker {
    UnresolvedReviewThreads(u64),
    ChecksNotForCurrentHead,
    CheckNotGreen { name: String, state: String },
    BaseConflict,
    MergeabilityUnknown,
}

pub struct PullRequestConvergence { /* private */ }
impl PullRequestConvergence {
    // accessors: is_converged(), blockers()
}

pub fn evaluate_pull_request_convergence(
    facts: &PullRequestConvergenceFacts,
) -> PullRequestConvergence;
```

## application: repo_watch

```rust
pub trait RepoWatchEventIdGenerator {
    fn next_event_id(&mut self) -> RepoWatchEventId;
}

pub struct UuidV7RepoWatchEventIdGenerator;

pub struct RepoWatchEventContentIdentityV1(/* private */);
impl RepoWatchEventContentIdentityV1 {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self;
    // accessors: as_bytes()
}

pub struct RepoWatchEventIdentityFrontierEntryV1 { /* private */ }
impl RepoWatchEventIdentityFrontierEntryV1 {
    pub const fn new(stream_identity: [u8; 32], sequence: NonZeroU64) -> Self;
    pub const fn for_pull_request(
        stream_identity: [u8; 32],
        sequence: NonZeroU64,
        pull_request_number: PullRequestNumber,
    ) -> Self;
    // accessors: stream_identity(), sequence(), pull_request_number()
}

pub struct RepoWatchEventIdentityFrontierV1 { /* private */ }
impl RepoWatchEventIdentityFrontierV1 {
    pub fn try_from_entries(
        entries: Vec<RepoWatchEventIdentityFrontierEntryV1>,
    ) -> Result<Self, RepoWatchEventIdentityFrontierError>;
    pub fn entries(
        &self,
    ) -> impl ExactSizeIterator<Item = RepoWatchEventIdentityFrontierEntryV1> + '_;
}

pub enum RepoWatchEventIdentityFrontierError {
    DuplicateStream,
    StreamLimit,
    SequenceExhausted,
}

pub struct RepoWatchEventOccurrenceV1 { /* private */ }
impl RepoWatchEventOccurrenceV1 {
    // Compiled only under the `test-support` feature.
    pub const fn from_parts(
        event: RepoWatchEvent,
        content_identity: RepoWatchEventContentIdentityV1,
    ) -> Self;
    // accessors: event(), content_identity()
    pub fn into_event(self) -> RepoWatchEvent;
}

pub enum RepoWatchPullRequestLifecycle {
    Open,
    Closed,
    Merged,
}

pub struct RepoWatchCheckCompletionGeneration { /* private */ }
impl RepoWatchCheckCompletionGeneration {
    pub fn try_new(value: String) -> Result<Self, RepoWatchCheckCompletionGenerationError>;
    // accessors: as_str()
}

pub struct RepoWatchCheckCompletionGenerationError;

pub struct RepoWatchCheckSuiteObservation { /* private */ }
impl RepoWatchCheckSuiteObservation {
    pub const fn new(
        id: GitHubObjectId,
        completion_generation: RepoWatchCheckCompletionGeneration,
        outcome: ChecksOutcome,
    ) -> Self;
    // accessors: id(), completion_generation(), outcome()
}

pub struct RepoWatchCheckRunObservation { /* private */ }
impl RepoWatchCheckRunObservation {
    pub const fn new(
        id: GitHubObjectId,
        completion_generation: RepoWatchCheckCompletionGeneration,
        name: CheckRunName,
        conclusion: CheckConclusion,
    ) -> Self;
    // accessors: id(), completion_generation(), name(), conclusion()
}

pub struct RepoWatchReviewObservation { /* private */ }
impl RepoWatchReviewObservation {
    pub const fn new(
        id: GitHubObjectId,
        reviewer: RepoWatchAuthorLogin,
        state: Option<ReviewState>,
        commit: CommitSha,
    ) -> Self;
    // accessors: id(), reviewer(), state(), commit()
}

pub enum RepoWatchReviewDecision {
    None,
    Approved,
    ReviewRequired,
    ChangesRequested,
}

pub enum RepoWatchConvergenceVerdict {
    NotConverged,
    InternallyConverged,
    MergeReady,
}

pub struct RepoWatchConvergenceAssessmentInput {
    pub number: PullRequestNumber,
    pub head_sha: CommitSha,
    pub base_branch: BranchName,
    pub base_revision: CommitSha,
    pub mergeable_state: MergeableState,
    pub settled: bool,
    pub review_decision: RepoWatchReviewDecision,
    pub unresolved_threads: Vec<ReviewThreadId>,
    pub gating_check_count: u64,
    pub non_green_gating_checks: Vec<CheckRunName>,
}

pub struct RepoWatchConvergenceAssessment { /* private */ }
impl RepoWatchConvergenceAssessment {
    pub fn try_new(
        input: RepoWatchConvergenceAssessmentInput,
    ) -> Result<Self, RepoWatchConvergenceAssessmentError>;
    // accessors: number(), head_sha(), base_branch(), base_revision(), mergeable_state(),
    // settled(), review_decision(), unresolved_threads(), gating_check_count(),
    // non_green_gating_checks(), verdict()
}

pub struct RepoWatchConvergenceAssessmentError;

pub struct RepoWatchStaleReviewClearanceCandidate { /* private */ }
impl RepoWatchStaleReviewClearanceCandidate {
    pub fn review_node_id_is_valid(value: &str) -> bool;
    pub fn try_new(
        assessment: &RepoWatchConvergenceAssessment,
        review_node_id: String,
        reviewer: RepoWatchAuthorLogin,
        reviewed_head_sha: CommitSha,
    ) -> Result<Self, RepoWatchStaleReviewClearanceCandidateError>;
    // accessors: number(), current_head_sha(), review_node_id(), reviewer(),
    // reviewed_head_sha()
}

pub struct RepoWatchStaleReviewClearanceCandidateError;

pub enum RepoWatchThreadState {
    Open,
    Resolved,
}

pub struct RepoWatchThreadObservation { /* private */ }
impl RepoWatchThreadObservation {
    pub const fn new(thread: ReviewThreadId, state: RepoWatchThreadState) -> Self;
    // accessors: thread(), state()
}

pub struct RepoWatchReactionObservation { /* private */ }
impl RepoWatchReactionObservation {
    pub const fn new(
        subject: ReactionSubject,
        reactor: RepoWatchAuthorLogin,
        content: ReactionContent,
    ) -> Self;
    // accessors: subject(), reactor(), content()
}

pub struct RepoWatchMergedCheckSuiteBaselineV1 { /* private */ }
impl RepoWatchMergedCheckSuiteBaselineV1 {
    pub const fn new(
        id: GitHubObjectId,
        completion_generation: RepoWatchCheckCompletionGeneration,
    ) -> Self;
    // accessors: id(), completion_generation()
}

pub struct RepoWatchMergedCheckRunBaselineV1 { /* private */ }
impl RepoWatchMergedCheckRunBaselineV1 {
    pub const fn new(
        id: GitHubObjectId,
        completion_generation: RepoWatchCheckCompletionGeneration,
        conclusion: CheckConclusion,
    ) -> Self;
    // accessors: id(), completion_generation(), conclusion()
}

pub struct RepoWatchMergedPullRequestBaselineInputV1 {
    pub number: PullRequestNumber,
    pub head_sha: CommitSha,
    pub signal_reviewers: Vec<RepoWatchAuthorLogin>,
    pub labels: Vec<LabelName>,
    pub mergeable_state: MergeableState,
    pub completed_check_suites: Vec<RepoWatchMergedCheckSuiteBaselineV1>,
    pub completed_check_runs: Vec<RepoWatchMergedCheckRunBaselineV1>,
    pub review_ids: Vec<GitHubObjectId>,
    pub threads: Vec<RepoWatchThreadObservation>,
    pub reactions: Vec<RepoWatchReactionObservation>,
}

pub struct RepoWatchMergedPullRequestBaselineV1 { /* private */ }
impl RepoWatchMergedPullRequestBaselineV1 {
    pub fn try_new(
        input: RepoWatchMergedPullRequestBaselineInputV1,
    ) -> Result<Self, RepoWatchRepositoryStateError>;
    pub fn from_merged_state(
        state: &RepoWatchPullRequestState,
        signal_reviewers: &[RepoWatchAuthorLogin],
    ) -> Result<Option<Self>, RepoWatchRepositoryStateError>;
    // accessors: number(), head_sha(), signal_reviewers(), labels(), mergeable_state(),
    // completed_check_suites(), completed_check_runs(), review_ids(), threads(), reactions()
}

pub struct RepoWatchPullRequestStateInput {
    pub context: PullRequestEventContext,
    pub lifecycle: RepoWatchPullRequestLifecycle,
    pub mergeable_state: MergeableState,
    pub completed_check_suites: Vec<RepoWatchCheckSuiteObservation>,
    pub completed_check_runs: Vec<RepoWatchCheckRunObservation>,
    pub reviews: Vec<RepoWatchReviewObservation>,
    pub threads: Vec<RepoWatchThreadObservation>,
    pub reactions: Vec<RepoWatchReactionObservation>,
}

pub struct RepoWatchPullRequestState { /* private */ }
impl RepoWatchPullRequestState {
    pub fn try_new(
        input: RepoWatchPullRequestStateInput,
    ) -> Result<Self, RepoWatchRepositoryStateError>;
    // accessors: context(), lifecycle(), mergeable_state(), completed_check_suites(),
    // completed_check_runs(), reviews(), threads(), reactions()
}

pub struct RepoWatchWorkflowRunObservation { /* private */ }
impl RepoWatchWorkflowRunObservation {
    pub const fn new(
        id: GitHubObjectId,
        workflow_id: GitHubObjectId,
        attempt: RepoWatchWorkflowRunAttempt,
        branch: BranchName,
        workflow: WorkflowName,
        conclusion: CheckConclusion,
    ) -> Self;
    // accessors: id(), workflow_id(), attempt(), branch(), workflow(), conclusion()
}

pub struct RepoWatchBranchHead { /* private */ }
impl RepoWatchBranchHead {
    pub const fn new(branch: BranchName, head: CommitSha) -> Self;
    // accessors: branch(), head()
}

pub struct RepoWatchRepositoryStateInput {
    pub pull_requests: Vec<RepoWatchPullRequestState>,
    pub workflow_runs: Vec<RepoWatchWorkflowRunObservation>,
    pub branch_heads: Vec<RepoWatchBranchHead>,
}

pub struct RepoWatchRepositoryState { /* private */ }
impl RepoWatchRepositoryState {
    pub fn try_new(
        input: RepoWatchRepositoryStateInput,
    ) -> Result<Self, RepoWatchRepositoryStateError>;
    // accessors: pull_requests(), workflow_runs(), branch_heads()
}

pub struct RepoWatchObservation { /* private */ }
impl RepoWatchObservation {
    pub fn new(
        signal_reviewers: Vec<RepoWatchAuthorLogin>,
        state: RepoWatchRepositoryState,
    ) -> Self;
    // accessors: signal_reviewers(), state()
}

pub enum RepoWatchRepositoryStateError {
    DuplicatePullRequest(PullRequestNumber),
    MergedPullRequestBaselineLimit,
    DuplicateCheckSuite(GitHubObjectId),
    DuplicateCheckRun(GitHubObjectId),
    DuplicateReview(GitHubObjectId),
    DuplicateThread(ReviewThreadId),
    DuplicateWorkflow { branch: BranchName, workflow_id: GitHubObjectId },
    DuplicateBranchHead(BranchName),
}

pub enum RepoWatchDifferFailureKind {
    BaselineCollection,
    EventConstruction,
    IdentityFrontier,
}

pub struct RepoWatchDifferError(/* private */);
impl RepoWatchDifferError {
    pub const fn kind(&self) -> RepoWatchDifferFailureKind;
}

pub fn repo_watch_events_have_equal_identified_content(
    left: &RepoWatchEvent,
    right: &RepoWatchEvent,
) -> bool;

pub fn derive_repo_watch_events(
    repository: &RepositorySlug,
    previous: Option<&RepoWatchObservation>,
    current: &RepoWatchObservation,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
) -> Result<Vec<RepoWatchEventOccurrenceV1>, RepoWatchDifferError>;

pub fn derive_repo_watch_events_with_merged_baselines(
    repository: &RepositorySlug,
    previous: Option<&RepoWatchObservation>,
    merged_baselines: &[RepoWatchMergedPullRequestBaselineV1],
    current: &RepoWatchObservation,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
) -> Result<Vec<RepoWatchEventOccurrenceV1>, RepoWatchDifferError>;

pub struct RepoWatchResolvedTemplate { /* private */ }
impl RepoWatchResolvedTemplate {
    pub const fn new(
        provenance: SessionTemplateProvenance,
        defaults: SessionConfigurationDefaults,
    ) -> Self;
    // accessors: provenance(), defaults()
}

pub trait RepoWatchTemplateResolver {
    fn resolve_repo_watch_template(
        &self,
        name: &SessionTemplateName,
    ) -> Option<RepoWatchResolvedTemplate>;
}

pub enum RepoWatchSingletonKey {
    PullRequest { repository: RepositorySlug, number: PullRequestNumber },
    Stack { repository: RepositorySlug, root_pull_request: PullRequestNumber },
    Rule,
    Repository { repository: RepositorySlug },
}

pub struct RepoWatchPreparedDispatchAction { /* private */ }
impl RepoWatchPreparedDispatchAction {
    // accessors: action(), prepared_session(), goal()
    pub fn into_parts(
        self,
    ) -> (
        RepoWatchActionV1,
        PreparedCreateSession,
        SubmitInput,
        GoalUserCommand,
    );
}

pub enum RepoWatchRuleEvaluation {
    NotMatched {
        event: RepoWatchEvent,
        rule_id: RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
    },
    Matched {
        dispatch_id: RepoWatchDispatchId,
        event: RepoWatchEvent,
        rule_id: RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
        singleton: RepoWatchSingletonKey,
        cooldown: std::time::Duration,
        actions: Box<[RepoWatchPreparedDispatchAction]>,
    },
}

pub enum RepoWatchRuleEvaluationOutcome {
    Inactive,
    NotMatched,
    TargetClosed,
    TargetConverged,
    Occupied,
    Cooldown,
    Dispatched {
        dispatch_id: RepoWatchDispatchId,
        sessions: Box<[SessionId]>,
    },
    Replayed {
        dispatch_id: RepoWatchDispatchId,
        sessions: Box<[SessionId]>,
    },
}

pub trait RepoWatchDispatchTransaction {
    type Error;
    async fn handle_repo_watch_evaluation(
        &mut self,
        evaluation: RepoWatchRuleEvaluation,
        ids: &mut (impl SubmitInputIdGenerator + Send),
    ) -> Result<RepoWatchRuleEvaluationOutcome, Self::Error>;
}

pub trait RepoWatchDispatchIdGenerator {
    fn next_dispatch_id(&mut self) -> RepoWatchDispatchId;
    fn next_command_id(&mut self) -> DurableCommandId;
    fn next_session_id(&mut self) -> SessionId;
}

pub struct UuidV7RepoWatchDispatchIdGenerator;

pub enum RepoWatchDispatchPreparationError {
    Context(RepoWatchDispatchContextError),
    UnknownTemplate(SessionTemplateName),
    SessionPreparation,
    InvalidSingletonTarget,
    GoalStatement(GoalTextError),
}

pub struct RepoWatchDispatchService<Ids, Transaction> { /* private */ }
impl<Ids, Transaction> RepoWatchDispatchService<Ids, Transaction> {
    pub const fn new(ids: Ids, transaction: Transaction) -> Self;
}
impl<Ids: RepoWatchDispatchIdGenerator, Transaction: RepoWatchDispatchTransaction>
    RepoWatchDispatchService<Ids, Transaction>
{
    pub async fn evaluate(
        &mut self,
        event: RepoWatchEvent,
        rule: &RepoWatchRule,
        observation: &RepoWatchObservation,
        templates: &impl RepoWatchTemplateResolver,
        context: UserContent,
    ) -> Result<
        RepoWatchRuleEvaluationOutcome,
        RepoWatchDispatchServiceError<Transaction::Error>,
    >;
}

pub enum RepoWatchDispatchServiceError<TransactionError> {
    Preparation(RepoWatchDispatchPreparationError),
    Transaction(TransactionError),
}
```

## application: repo_watch_webhook

```rust
pub struct RepoWatchWebhookBodyReferenceV1 { /* private */ }
impl RepoWatchWebhookBodyReferenceV1 {
    pub const fn new(hook_id: NonZeroU64, delivery_id: Uuid) -> Self;
    // accessors: hook_id(), delivery_id()
}

pub struct RepoWatchWebhookDeliveryV1Input {
    pub repository: RepositorySlug,
    pub hook_id: NonZeroU64,
    pub delivery_id: Uuid,
    pub event: String,
    pub action: Option<String>,
    pub receipt_sequence: NonZeroU64,
    pub body_digest: [u8; 32],
}

pub struct RepoWatchWebhookDeliveryV1 { /* private */ }
impl RepoWatchWebhookDeliveryV1 {
    pub fn new(input: RepoWatchWebhookDeliveryV1Input) -> Self;
    // accessors: repository(), hook_id(), delivery_id(), event(), action(),
    // receipt_sequence(), body_digest(), body_reference()
}

pub enum RepoWatchPullRequestMissingPolicyV1 {
    HydrateBeforeApplying,
    RefreshInstead,
}

pub enum RepoWatchPullRequestHeadGuardV1 {
    AbsentOrMatching(CommitSha),
    Expected(CommitSha),
}

pub struct RepoWatchWebhookPullRequestContextV1Input {
    pub number: PullRequestNumber,
    pub head_sha: CommitSha,
    pub head_repository: Option<RepositorySlug>,
    pub base_branch: BranchName,
    pub head_branch: BranchName,
    pub title: PullRequestTitle,
    pub body: PullRequestBody,
    pub labels: Vec<LabelName>,
    pub draft: bool,
    pub author: Option<RepoWatchAuthorLogin>,
}

pub struct RepoWatchWebhookPullRequestContextV1 { /* private */ }
impl RepoWatchWebhookPullRequestContextV1 {
    pub fn new(input: RepoWatchWebhookPullRequestContextV1Input) -> Self;
    pub fn delivered(&self) -> Option<PullRequestEventContext>;
    pub fn with_retained_head_repository(
        &self,
        retained: &RepositorySlug,
    ) -> PullRequestEventContext;
    // accessors: number(), head_sha(), head_repository()
}

pub enum RepoWatchObservationChangeV1 {
    PullRequestContext {
        context: RepoWatchWebhookPullRequestContextV1,
        lifecycle: Option<RepoWatchPullRequestLifecycle>,
        head_guard: RepoWatchPullRequestHeadGuardV1,
        missing: RepoWatchPullRequestMissingPolicyV1,
    },
    ReviewUnion {
        pull_request: PullRequestNumber,
        expected_head: CommitSha,
        review: RepoWatchReviewObservation,
    },
    ThreadState {
        pull_request: PullRequestNumber,
        expected_head: CommitSha,
        thread: RepoWatchThreadObservation,
    },
    CheckRunUnion {
        pull_request: PullRequestNumber,
        expected_head: CommitSha,
        check_run: RepoWatchCheckRunObservation,
    },
    WorkflowRun { run: RepoWatchWorkflowRunObservation },
    BranchHead {
        previous: RepoWatchBranchHeadPreviousV1,
        current: RepoWatchBranchHead,
    },
    BranchDeleted {
        branch: BranchName,
        expected_previous: CommitSha,
    },
}

pub enum RepoWatchBranchHeadPreviousV1 {
    Absent,
    Expected(CommitSha),
}

pub enum RepoWatchTargetedRefreshV1 {
    PullRequestHydration { pull_request: PullRequestNumber },
    Mergeability {
        pull_request: PullRequestNumber,
        expected_head: CommitSha,
    },
    CheckRollup {
        pull_request: PullRequestNumber,
        expected_head: CommitSha,
    },
    CheckRollupForCommit { head: CommitSha },
}

pub struct RepoWatchTargetedRefreshCoalescerV1 { /* private */ }
impl RepoWatchTargetedRefreshCoalescerV1 {
    pub fn for_delivery_page() -> Self;
    pub fn unissued(&self, refreshes: &[RepoWatchTargetedRefreshV1]) -> Vec<RepoWatchTargetedRefreshV1>;
    pub fn record_issued(&mut self, refreshes: &[RepoWatchTargetedRefreshV1]);
}

pub struct RepoWatchObservationPatchV1 { /* private */ }
impl RepoWatchObservationPatchV1 {
    // accessors: changes(), targeted_refreshes()
}

pub enum RepoWatchObservationApplyV1 {
    Applied(RepoWatchObservation),
    DuplicateState,
    Superseded,
    Ignored(RepoWatchWebhookIgnoredReasonV1),
    NeedsTargetedRefresh {
        observation: RepoWatchObservation,
        refreshes: Box<[RepoWatchTargetedRefreshV1]>,
    },
}

pub enum RepoWatchWebhookApplyError {
    RepositoryState(RepoWatchRepositoryStateError),
    ConflictingImmutableFact(&'static str),
}

pub enum RepoWatchWebhookMappedNoChangeV1 {
    Ping,
    ReviewDismissed,
}

pub enum RepoWatchWebhookIgnoredReasonV1 {
    UnmappedEvent,
    UnmappedAction,
    NonBranchPush,
    ForeignWorkflowRepository,
    AbsentWorkflowBranch,
    AbsentWorkflowHeadRepository,
    AbsentWorkflowHeadBranch,
}

pub enum RepoWatchWebhookMappingV1 {
    Patch(RepoWatchObservationPatchV1),
    MappedNoChange(RepoWatchWebhookMappedNoChangeV1),
    Ignored(RepoWatchWebhookIgnoredReasonV1),
}

pub enum RepoWatchWebhookMappingError {
    MalformedJson,
    MissingField(&'static str),
    InvalidField(&'static str),
    RepositoryMismatch,
    ActionMismatch,
}

pub fn apply_repo_watch_observation_patch_v1(
    previous: &RepoWatchObservation,
    patch: &RepoWatchObservationPatchV1,
) -> Result<RepoWatchObservationApplyV1, RepoWatchWebhookApplyError>;

pub fn map_repo_watch_webhook_delivery_v1(
    delivery: &RepoWatchWebhookDeliveryV1,
    exact_body: &[u8],
) -> Result<RepoWatchWebhookMappingV1, RepoWatchWebhookMappingError>;
```

## application: review_orchestration

```rust
pub struct ReviewOrchestrationAttemptId(/* private UUID */);
impl ReviewOrchestrationAttemptId {
    pub const fn from_uuid(value: uuid::Uuid) -> Self;
    pub const fn as_uuid(self) -> uuid::Uuid;
}

pub struct ReviewTemplateDigest(/* private 32 bytes */);
impl ReviewTemplateDigest {
    pub const fn new(bytes: [u8; 32]) -> Self;
    pub const fn bytes(self) -> [u8; 32];
}

pub struct ReviewStageTemplateDigests { /* private */ }
impl ReviewStageTemplateDigests {
    pub const fn new(
        import: ReviewTemplateDigest,
        judgment: ReviewTemplateDigest,
        repair: ReviewTemplateDigest,
        publication: ReviewTemplateDigest,
    ) -> Self;
    // accessors: import(), judgment(), repair(), publication()
}

pub struct ReviewConcernSpec { /* private */ }
impl ReviewConcernSpec {
    pub const fn new(key: ReviewKey, template_digest: ReviewTemplateDigest) -> Self;
    // accessors: key(), template_digest()
}

pub struct ReviewOrchestrationAttempt { /* private immutable attempt */ }
impl ReviewOrchestrationAttempt {
    pub fn try_new(
        id: ReviewOrchestrationAttemptId,
        target: ReviewTargetId,
        policy: ReviewPolicy,
        concern_set_version: ReviewKey,
        stage_templates: ReviewStageTemplateDigests,
        concerns: Vec<ReviewConcernSpec>,
    ) -> Result<Self, ReviewOrchestrationAttemptError>;
    // accessors: id(), target(), policy(), concern_set_version(),
    //   stage_templates(), concerns()
}
pub enum ReviewOrchestrationAttemptError {
    EmptyConcernInventory,
    RepeatedConcern { concern: ReviewKey },
}

pub enum ReviewDurableSealOutcome {
    Recorded,
    EqualReplay,
    Conflict,
}
pub enum ReviewPassIncompleteStatus {
    Failed,
    Blocked,
    Cancelled,
}
pub struct ReviewImportedContextEvidence { /* producing pass + digest */ }
impl ReviewImportedContextEvidence {
    pub const fn new(producer: ReviewPassRef, digest: [u8; 32]) -> Self;
    // accessors: producer(), digest()
}
pub enum ReviewImportOutcome {
    Succeeded {
        pass: Box<ReviewPassEvidence>,
        run: ReviewRunEvidence,
        external_link: Option<Box<ReviewExternalLink>>,
        template_digest: ReviewTemplateDigest,
        context: ReviewImportedContextEvidence,
    },
    Incomplete {
        pass: Option<Box<ReviewPassEvidence>>,
        run: Option<ReviewRunEvidence>,
        template_digest: ReviewTemplateDigest,
        status: ReviewPassIncompleteStatus,
    },
}
pub enum ReviewImportEvidenceFailure {
    ForeignTarget,
    ForeignPolicy,
    ForeignTemplate,
    IncompatibleContext,
    IncompatiblePass,
}

pub struct ReviewConcernSuccess { /* private */ }
impl ReviewConcernSuccess {
    pub fn new(
        producer: ReviewPassEvidence,
        run: ReviewRunEvidence,
        template_digest: ReviewTemplateDigest,
        findings: Vec<ReviewFinding>,
    ) -> Self;
    // accessors: producer(), producer_evidence(), run_evidence(), template_digest(), findings()
}
pub enum ReviewConcernOutcome {
    Succeeded(Box<ReviewConcernSuccess>),
    Failed { pass: ReviewPassRef },
    Blocked { pass: ReviewPassRef },
    Cancelled { pass: Option<ReviewPassRef> },
    Superseded { pass: ReviewPassRef },
}
pub struct ReviewConcernClaim { /* private */ }
impl ReviewConcernClaim {
    pub const fn new(
        concern: ReviewKey,
        template_digest: ReviewTemplateDigest,
        outcome: ReviewConcernOutcome,
    ) -> Self;
    // accessors: concern(), template_digest(), outcome()
}
pub struct ReviewConcernWork { /* private; produced by service */ }
impl ReviewConcernWork {
    // accessors: attempt(), imported_context_digest(), concern()
}
pub enum ReviewFanoutBarrierFailure {
    MissingConcern { concern: ReviewKey },
    ExtraConcern { concern: ReviewKey },
    RepeatedConcern { concern: ReviewKey },
    TemplateMismatch { concern: ReviewKey },
    MemberIncomplete { concern: ReviewKey },
    ForeignProducerTarget { concern: ReviewKey },
    ForeignProducerPolicy { concern: ReviewKey },
    ForeignProducerTemplate { concern: ReviewKey },
    InvalidSealedFinding {
        concern: ReviewKey,
        finding: ReviewFindingRef,
    },
    RepeatedFinding { finding: ReviewFindingRef },
}

pub enum ReviewPlannedDisposition {
    Accepted,
    Rejected { reason: ReviewText },
    Duplicate { canonical: ReviewFindingRef },
    Superseded { successor: ReviewFindingRef },
    Stale,
}
pub struct ReviewJudgmentPlanMember { /* private */ }
impl ReviewJudgmentPlanMember {
    pub const fn new(
        finding: ReviewFindingRef,
        disposition: ReviewPlannedDisposition,
    ) -> Self;
    // accessors: finding(), disposition()
}
pub struct ReviewJudgmentPlan { /* private */ }
impl ReviewJudgmentPlan {
    pub fn new(
        analysis_pass: ReviewPassEvidence,
        analysis_run: ReviewRunEvidence,
        template_digest: ReviewTemplateDigest,
        members: Vec<ReviewJudgmentPlanMember>,
    ) -> Self;
    // accessors: analysis_pass(), analysis_pass_evidence(),
    //   analysis_run_evidence(), template_digest(), members()
}
pub enum ReviewJudgmentPlanFailure {
    ForeignAnalysisTarget,
    ForeignAnalysisPolicy,
    ForeignAnalysisTemplate,
    IncompatibleAnalysisPass,
    InexactFindingInventory,
    AcceptedBelowThreshold { finding: ReviewFindingRef },
    InvalidReferencedFinding { finding: ReviewFindingRef },
    ReferenceCycle { finding: ReviewFindingRef },
    ReferencedFindingTerminalBeforeAdmission { finding: ReviewFindingRef },
}

pub struct ReviewJudgmentEffectId { /* attempt + finding */ }
impl ReviewJudgmentEffectId {
    pub const fn new(
        attempt: ReviewOrchestrationAttemptId,
        finding: ReviewFindingRef,
    ) -> Self;
    // accessors: attempt(), finding()
}
pub struct ReviewJudgmentEffectWork { /* private; produced by service */ }
impl ReviewJudgmentEffectWork {
    // accessors: id(), attempt(), member()
}
pub struct ReviewJudgmentEffectSuccess { /* private; event + template */ }
impl ReviewJudgmentEffectSuccess {
    pub const fn new(
        event: ReviewFindingEvent,
        template_digest: ReviewTemplateDigest,
    ) -> Self;
    // accessors: event(), template_digest()
}
pub enum ReviewJudgmentEffectOutcome {
    Applied(Box<ReviewJudgmentEffectSuccess>),
    Failed,
    Blocked,
    Cancelled,
}
pub enum ReviewJudgmentEffectEvidenceFailure {
    ForeignTarget,
    ForeignPolicy,
    ForeignTemplate,
    IncompatibleEvent,
    IncompatiblePass,
    IncompatibleRun,
}
pub struct ReviewRepairSuccess { /* private; fixed event + template */ }
impl ReviewRepairSuccess {
    pub const fn new(
        event: ReviewFindingEvent,
        template_digest: ReviewTemplateDigest,
    ) -> Self;
    // accessors: finding(), event(), template_digest()
}
pub enum ReviewRepairMemberOutcome {
    Fixed(Box<ReviewRepairSuccess>),
    Failed(ReviewFindingRef),
    Cancelled(ReviewFindingRef),
    Blocked(ReviewFindingRef),
}
pub struct ReviewRepairWork { /* private; produced by service */ }
impl ReviewRepairWork {
    // accessors: attempt(), findings()
}
pub struct ReviewPublicationSuccess { /* private; canonical link + run + template */ }
impl ReviewPublicationSuccess {
    pub const fn new(
        link: ReviewFindingExternalLinkRef,
        run: ReviewRunEvidence,
        template_digest: ReviewTemplateDigest,
    ) -> Self;
    // accessors: finding(), link(), run(), template_digest()
}
pub enum ReviewPublicationMemberOutcome {
    Published(Box<ReviewPublicationSuccess>),
    Failed(ReviewFindingRef),
    Blocked(ReviewFindingRef),
    Cancelled(ReviewFindingRef),
}
pub struct ReviewPublicationWork { /* private; produced by service */ }
impl ReviewPublicationWork {
    // accessors: attempt(), findings()
}
pub enum ReviewTerminalBarrierFailure {
    InexactFindingInventory,
    ForeignRepairTarget,
    ForeignRepairPolicy,
    ForeignRepairTemplate,
    IncompatibleRepairPass,
    IncompatibleRepairRun,
    IncompatibleRepairEvent,
    ForeignPublicationTarget,
    ForeignPublicationPolicy,
    ForeignPublicationTemplate,
    IncompatiblePublicationPass,
    IncompatiblePublicationRun,
    IncompatiblePublicationAttachment,
}

pub trait ReviewOrchestrationAttemptStore {
    type Error;
    fn record_attempt(&mut self, attempt: ReviewOrchestrationAttempt)
        -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn load_import(&self, attempt: ReviewOrchestrationAttemptId)
        -> impl Future<Output = Result<Option<ReviewImportOutcome>, Self::Error>> + Send;
    fn record_import(&mut self, attempt: ReviewOrchestrationAttemptId, outcome: ReviewImportOutcome)
        -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn load_concern_claims(&self, attempt: ReviewOrchestrationAttemptId)
        -> impl Future<Output = Result<Vec<ReviewConcernClaim>, Self::Error>> + Send;
    fn record_concern_claim(&mut self, attempt: ReviewOrchestrationAttemptId, claim: ReviewConcernClaim)
        -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn seal_complete_fanout(&mut self, attempt: ReviewOrchestrationAttemptId, claims: Vec<ReviewConcernClaim>)
        -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn seal_judgment_plan(&mut self, attempt: ReviewOrchestrationAttemptId, plan: ReviewJudgmentPlan)
        -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn load_judgment_plan(&self, attempt: ReviewOrchestrationAttemptId)
        -> impl Future<Output = Result<Option<ReviewJudgmentPlan>, Self::Error>> + Send;
    fn load_applied_judgment_effects(&self, attempt: ReviewOrchestrationAttemptId)
        -> impl Future<Output = Result<Vec<ReviewJudgmentEffectId>, Self::Error>> + Send;
    fn record_applied_judgment_effect(&mut self, effect: ReviewJudgmentEffectId)
        -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn seal_repair_inventory(&mut self, attempt: ReviewOrchestrationAttemptId, findings: Vec<ReviewFindingRef>)
        -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn record_repair_outcomes(&mut self, attempt: ReviewOrchestrationAttemptId, outcomes: Vec<ReviewRepairMemberOutcome>)
        -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn load_repair_outcomes(&self, attempt: ReviewOrchestrationAttemptId)
        -> impl Future<Output = Result<Option<Vec<ReviewRepairMemberOutcome>>, Self::Error>> + Send;
    fn seal_publication_inventory(&mut self, attempt: ReviewOrchestrationAttemptId, findings: Vec<ReviewFindingRef>)
        -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn record_publication_outcomes(&mut self, attempt: ReviewOrchestrationAttemptId, outcomes: Vec<ReviewPublicationMemberOutcome>)
        -> impl Future<Output = Result<ReviewDurableSealOutcome, Self::Error>> + Send;
    fn load_publication_outcomes(&self, attempt: ReviewOrchestrationAttemptId)
        -> impl Future<Output = Result<Option<Vec<ReviewPublicationMemberOutcome>>, Self::Error>> + Send;
}

pub trait ReviewOrchestrationPassRunner: Send + Sync + 'static {
    type Error: Send + 'static;
    fn import_external_context(&self, attempt: ReviewOrchestrationAttempt)
        -> impl Future<Output = Result<ReviewImportOutcome, Self::Error>> + Send;
    fn run_concern(&self, work: ReviewConcernWork)
        -> impl Future<Output = Result<ReviewConcernOutcome, Self::Error>> + Send + 'static;
    fn judge(&self, attempt: ReviewOrchestrationAttempt, findings: Vec<ReviewFinding>)
        -> impl Future<Output = Result<ReviewJudgmentPlan, Self::Error>> + Send;
    fn apply_judgment_effect(&self, work: ReviewJudgmentEffectWork)
        -> impl Future<Output = Result<ReviewJudgmentEffectOutcome, Self::Error>> + Send;
    fn repair(&self, work: ReviewRepairWork)
        -> impl Future<Output = Result<Vec<ReviewRepairMemberOutcome>, Self::Error>> + Send;
    fn publish(&self, work: ReviewPublicationWork)
        -> impl Future<Output = Result<Vec<ReviewPublicationMemberOutcome>, Self::Error>> + Send;
}

pub enum ReviewOrchestrationOutcome {
    ImportIncomplete(Box<ReviewImportOutcome>),
    FanoutIncomplete(ReviewFanoutBarrierFailure),
    JudgmentIncomplete {
        effect: ReviewJudgmentEffectId,
        outcome: ReviewJudgmentEffectOutcome,
    },
    RepairIncomplete { repairs: Vec<ReviewRepairMemberOutcome> },
    PublicationIncomplete { publications: Vec<ReviewPublicationMemberOutcome> },
    Complete { publications: Vec<ReviewPublicationMemberOutcome> },
}
pub enum ReviewOrchestrationServiceError<StoreError, RunnerError> {
    Store(StoreError),
    InvalidImportEvidence(ReviewImportEvidenceFailure),
    InvalidConcernEvidence(ReviewFanoutBarrierFailure),
    Runner(RunnerError),
    ConcernTaskTerminated,
    DurableConflict,
    InvalidJudgmentPlan(ReviewJudgmentPlanFailure),
    InvalidJudgmentEffectEvidence(ReviewJudgmentEffectEvidenceFailure),
    InvalidAppliedEffects,
    InvalidTerminalBarrier(ReviewTerminalBarrierFailure),
}
pub struct ReviewOrchestrationService<Store, Runner> { /* private */ }
impl<Store, Runner> ReviewOrchestrationService<Store, Runner> {
    pub fn new(store: Store, runner: Runner) -> Self;
}
impl<Store, Runner> ReviewOrchestrationService<Store, Runner>
where
    Store: ReviewOrchestrationAttemptStore,
    Runner: ReviewOrchestrationPassRunner,
{
    pub async fn execute(
        &mut self,
        attempt: ReviewOrchestrationAttempt,
    ) -> Result<
        ReviewOrchestrationOutcome,
        ReviewOrchestrationServiceError<Store::Error, Runner::Error>,
    >;
}
```

## application: review_workflow

```rust
pub struct ReviewWorkflowCommand { /* private */ }
impl ReviewWorkflowCommand {
    pub const fn new(
        command_id: DurableCommandId,
        semantic_digest: [u8; 32],
        operation: ReviewWorkflowOperation,
    ) -> Self;
    // accessors: command_id(), semantic_digest(), operation()
}

pub enum ReviewWorkflowOperation {
    CreateTarget(ReviewTarget),
    StartRun { run: ReviewRun, pass: ReviewPass },
    ActivatePass { run: ReviewRun, pass: ReviewPass },
    CompletePass { run: ReviewRun, pass: ReviewPass },
    RecordFindings {
        pass: ReviewPassEvidence,
        findings: Vec<ReviewFinding>,
    },
    RecordFindingEvent {
        pass: ReviewPassEvidence,
        event: ReviewFindingEvent,
    },
    ReserveExternalLink(ReviewExternalLink),
    AttachExternalLink {
        link: ReviewExternalLinkId,
        attachment: ReviewExternalLinkAttachment,
    },
}

impl ReviewWorkflowOperation {
    pub const fn kind(&self) -> ReviewWorkflowOperationKind;
}

pub enum ReviewWorkflowOperationKind {
    CreateTarget,
    StartRun,
    ActivatePass,
    CompletePass,
    RecordFindings,
    RecordFindingEvent,
    ReserveExternalLink,
    AttachExternalLink,
}

pub enum ReviewPassCompletionStatus {
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}
impl ReviewPassCompletionStatus {
    pub const fn from_state(state: &ReviewPassState) -> Option<Self>;
}

pub enum ReviewWorkflowCommandResult {
    TargetCreated { target: ReviewTargetId },
    RunStarted { run: ReviewRunId, pass: ReviewPassId },
    PassActivated { run: ReviewRunId, pass: ReviewPassId },
    PassCompleted {
        run: ReviewRunId,
        pass: ReviewPassId,
        status: ReviewPassCompletionStatus,
    },
    FindingsRecorded {
        run: ReviewRunId,
        pass: ReviewPassId,
        finding_count: usize,
    },
    FindingEventRecorded {
        finding: ReviewFindingId,
        status: ReviewFindingStatus,
    },
    ExternalLinkReserved { link: ReviewExternalLinkId },
    ExternalLinkAttached {
        link: ReviewExternalLinkId,
        external_object: ReviewKey,
    },
}

pub enum ReviewWorkflowCommandOutcome {
    Recorded(ReviewWorkflowCommandResult),
    ConflictingReuse { command_id: DurableCommandId },
}

pub trait ReviewWorkflowTransaction {
    type Error;

    fn handle(
        &mut self,
        command: ReviewWorkflowCommand,
    ) -> impl Future<Output = Result<ReviewWorkflowCommandOutcome, Self::Error>> + Send;
}

pub struct ReviewWorkflowCommandService<Transaction> { /* private */ }
impl<Transaction> ReviewWorkflowCommandService<Transaction> {
    pub const fn new(transaction: Transaction) -> Self;
}
impl<Transaction: ReviewWorkflowTransaction> ReviewWorkflowCommandService<Transaction> {
    pub async fn execute(
        &mut self,
        command: ReviewWorkflowCommand,
    ) -> Result<ReviewWorkflowCommandOutcome, Transaction::Error>;
}

pub trait ReviewWorkflowReader {
    type Error;

    fn load_target(
        &self,
        target: ReviewTargetId,
    ) -> impl Future<Output = Result<Option<ReviewTarget>, Self::Error>> + Send;
    fn load_run(
        &self,
        run: ReviewRunId,
    ) -> impl Future<Output = Result<Option<ReviewRun>, Self::Error>> + Send;
    fn load_run_with_pass(
        &self,
        run: ReviewRunId,
    ) -> impl Future<Output = Result<Option<(ReviewRun, Option<ReviewPass>)>, Self::Error>> + Send;
    fn load_pass(
        &self,
        pass: ReviewPassId,
    ) -> impl Future<Output = Result<Option<ReviewPass>, Self::Error>> + Send;
    fn load_finding(
        &self,
        finding: ReviewFindingId,
    ) -> impl Future<Output = Result<Option<ReviewFinding>, Self::Error>> + Send;
    fn list_findings(
        &self,
        run: ReviewRunId,
    ) -> impl Future<Output = Result<Vec<ReviewFinding>, Self::Error>> + Send;
}
```

## application: session_metadata

```rust
pub struct ReplaceSessionMetadataRequest { /* private */ }
impl ReplaceSessionMetadataRequest {
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        replacement: SessionMetadataContent,
    ) -> Result<Self, InvalidDurableCommandId>;
    pub fn try_new_for_tool(
        command_id: DurableCommandId,
        session: SessionId,
        request: ToolRequestId,
        replacement: SessionMetadataContent,
    ) -> Result<Self, InvalidDurableCommandId>;
    // accessors: command_id(), session(), actor(), replacement()
}

pub enum ReplaceSessionMetadataOutcome {
    Recorded(ReplaceSessionMetadataResult),
    ConflictingReuse { command_id: DurableCommandId },
}

pub trait ReplaceSessionMetadataTransaction {
    type Error;
    fn handle(
        &mut self,
        command: ReplaceSessionMetadata,
    ) -> impl Future<Output = Result<ReplaceSessionMetadataOutcome, Self::Error>> + Send;
}

pub struct ReplaceSessionMetadataService<Transaction> { /* private */ }
impl<Transaction> ReplaceSessionMetadataService<Transaction> {
    pub const fn new(transaction: Transaction) -> Self;
    pub fn into_transaction(self) -> Transaction;
}
impl<Transaction: ReplaceSessionMetadataTransaction> ReplaceSessionMetadataService<Transaction> {
    pub async fn execute(
        &mut self,
        request: ReplaceSessionMetadataRequest,
    ) -> Result<ReplaceSessionMetadataOutcome, Transaction::Error>;
}

pub trait SessionMetadataReader {
    type Error;
    fn load_session_metadata(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<Option<SessionMetadataSnapshot>, Self::Error>> + Send;
}

pub struct LoadSessionMetadataService<Reader> { /* private */ }
impl<Reader> LoadSessionMetadataService<Reader> {
    pub const fn new(reader: Reader) -> Self;
    pub fn into_reader(self) -> Reader;
}
impl<Reader: SessionMetadataReader> LoadSessionMetadataService<Reader> {
    pub async fn execute(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionMetadataSnapshot>, Reader::Error>;
}

pub struct SessionMetadataListQuery { /* private */ }
impl SessionMetadataListQuery {
    pub fn default_page(page_size: u64) -> Self;
    pub fn try_new(
        required_tags: Vec<String>,
        title_contains: Option<String>,
        include_archived: bool,
        page_size: u64,
        after_session: Option<SessionId>,
    ) -> Result<Self, SessionMetadataListQueryError>;
    pub fn try_new_with_required_tag_limit(
        required_tags: Vec<String>,
        title_contains: Option<String>,
        include_archived: bool,
        page_size: u64,
        after_session: Option<SessionId>,
        max_required_tags: Option<usize>,
    ) -> Result<Self, SessionMetadataListQueryError>;
    pub fn try_new_with_limits(
        required_tags: Vec<String>,
        title_contains: Option<String>,
        include_archived: bool,
        page_size: u64,
        after_session: Option<SessionId>,
        max_required_tags: Option<usize>,
        minimum_page_size: Option<u64>,
        maximum_page_size: Option<u64>,
    ) -> Result<Self, SessionMetadataListQueryError>;
    pub fn required_tags(&self) -> impl ExactSizeIterator<Item = &str>;
    // accessors: title_contains(), include_archived(), page_size(), after_session()
}

pub enum SessionMetadataListQueryError {
    TooManyRequiredTags,
    EmptyTag,
    TagContainsNul,
    TagExceedsIndexedUtf8Bytes,
    DuplicateTag,
    EmptyTitleSearch,
    TitleSearchContainsNul,
    TotalUtf8BytesExceeded,
    PageSizeOutOfRange,
}

pub struct SessionMetadataListItem { /* private */ }
impl SessionMetadataListItem {
    pub fn new(
        snapshot: &SessionMetadataSnapshot,
        defaults_version: SessionConfigurationDefaultsVersion,
        model_selection: ModelSelectionRequest,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
    ) -> Self;
    pub fn title(&self) -> Option<&str>;
    pub fn tags(&self) -> impl ExactSizeIterator<Item = &str>;
    // accessors: session(), defaults_version(), model_selection(),
    // dangerous_tool_auto_approval(), archived(), last_writer()
}

pub trait SessionMetadataPageReader {
    type Error;
    fn next_item(
        &mut self,
    ) -> impl Future<Output = Result<Option<SessionMetadataListItem>, Self::Error>> + Send;
    fn next_after_session(&self) -> Option<SessionId>;
}

pub trait SessionMetadataLister {
    type Error;
    type Page: SessionMetadataPageReader<Error = Self::Error>;
    fn open_session_metadata_page(
        &self,
        query: SessionMetadataListQuery,
    ) -> impl Future<Output = Result<Self::Page, Self::Error>> + Send;
}

pub struct ListSessionMetadataService<Lister> { /* private */ }
impl<Lister> ListSessionMetadataService<Lister> {
    pub const fn new(lister: Lister) -> Self;
    pub fn into_lister(self) -> Lister;
}
impl<Lister: SessionMetadataLister> ListSessionMetadataService<Lister> {
    pub async fn execute(
        &self,
        query: SessionMetadataListQuery,
    ) -> Result<Lister::Page, Lister::Error>;
}
```

## application: workspace_instructions

```rust
pub struct InstructionDiscoveryRoot { /* private */ }
impl InstructionDiscoveryRoot {
    pub const fn new(kind: InstructionDiscoveryRootKind, path: InstructionPath) -> Self;
    // accessors: kind(), path()
}
pub enum InstructionDiscoveryFindingKind {
    RootUnavailable,
    EntryUnreadable,
    NonUtf8SourcePath,
    NonUtf8Source,
    InvalidSkill,
    LimitReached(InstructionDiscoveryLimitKind),
}
pub enum InstructionDiscoveryLimitKind {
    ClassifiedEntries,
    Findings,
    CandidateSourceBytes,
    ElapsedTime,
}
pub struct InstructionDiscoveryFinding { /* private */ }
impl InstructionDiscoveryFinding {
    // accessors: path(), kind()
}
pub struct InstructionDiscoverySnapshot { /* private */ }
impl InstructionDiscoverySnapshot {
    // accessors: roots(), bundles(), findings(), limit_set_version(),
    // classified_entries(), candidate_source_bytes(), elapsed_millis(), is_complete()
}
pub fn discover_workspace_instructions(
    roots: Vec<InstructionDiscoveryRoot>,
) -> InstructionDiscoverySnapshot;
```

## application: operator_failure

```rust
pub enum OperatorFailureClass {
    Infrastructure { commit_ambiguous: bool },
    FailClosedCorruption,
    IdentityCollision,
    CallerOrHubBug,
}

pub trait ClassifyOperatorFailure {
    fn operator_failure_class(&self) -> OperatorFailureClass;
    fn operator_failure_cause_code(&self) -> &'static str;
}
```

## application: session_delegation

```rust
pub trait DelegationMessageDeliveryProjection {
    fn tool_request(&self) -> ToolRequestId;
    fn message(&self) -> DelegationMessageId;
    fn direction(&self) -> DelegationMessageDirection;
    fn ordinal(&self) -> DelegationEventOrdinal;
    fn delivery_sequence(&self) -> NonZeroU64;
}
```

## application: start_eligible_turn

```rust
pub trait StartEligibleTurnIdGenerator {
    fn next_model_identity_entry_id(&mut self) -> SemanticTranscriptEntryId;
    fn next_origin_entry_id(&mut self) -> SemanticTranscriptEntryId;
    fn next_starting_frontier_id(&mut self) -> ContextFrontierId;
    fn next_initial_attempt_id(&mut self) -> TurnAttemptId;
}

pub struct UuidV7StartEligibleTurnIdGenerator;
// Default; impl StartEligibleTurnIdGenerator

pub enum StartEligibleTurnOutcome {
    NoEligibleTurn,
    Activated(Box<ActivatedTurn>),
}

pub trait StartEligibleTurnTransaction {
    type Error;

    fn handle(
        &mut self,
        session: SessionId,
        identities: AcceptedInputTurnActivationIdentities,
    ) -> impl Future<Output = Result<StartEligibleTurnOutcome, Self::Error>> + Send;
}

pub struct StartEligibleTurnService<Generator, Transaction> { /* private */ }
// Clone when both ports are Clone
impl<Generator, Transaction> StartEligibleTurnService<Generator, Transaction> {
    pub const fn new(ids: Generator, transaction: Transaction) -> Self;
    pub fn into_parts(self) -> (Generator, Transaction);
}
impl<
    Generator: StartEligibleTurnIdGenerator,
    Transaction: StartEligibleTurnTransaction,
> StartEligibleTurnService<Generator, Transaction> {
    pub async fn execute(
        &mut self,
        session: SessionId,
    ) -> Result<StartEligibleTurnOutcome, Transaction::Error>;
    pub fn execute_with_cloned_transaction(
        &mut self,
        session: SessionId,
    ) -> impl Future<
        Output = Result<StartEligibleTurnOutcome, Transaction::Error>,
    > + Send
           + 'static
    where
        Transaction: Clone + Send + 'static,
        Transaction::Error: Send + 'static;
    pub fn execute_with_cloned_transaction_and_observer(
        &mut self,
        session: SessionId,
        observer: Arc<dyn Fn(TurnId) + Send + Sync>,
    ) -> impl Future<
        Output = Result<StartEligibleTurnOutcome, Transaction::Error>,
    > + Send
           + 'static
    where
        Transaction: Clone + Send + 'static,
        Transaction::Error: Send + 'static;
}
```

## application: scheduler

```rust
pub const fn scheduler_ordinary_pass_limit(max_in_flight_passes: usize) -> usize;

pub struct ReconciliationSweepInterval(/* private */);
impl ReconciliationSweepInterval {
    pub fn try_new(
        interval: Duration,
    ) -> Result<Self, InvalidReconciliationSweepInterval>;
    pub const fn get(self) -> Duration;
}

pub struct InvalidReconciliationSweepInterval;
// impl Display + std::error::Error

pub enum EligibilityNudgeOutcome {
    Enqueued,
    Coalesced,
    DroppedAtCapacity,
    WorkSourceClosed,
}

pub trait EligibilityNudge {
    fn nudge(&self, session: SessionId) -> EligibilityNudgeOutcome;
    fn nudge_dispatch_start(&self, session: SessionId) -> EligibilityNudgeOutcome;
}

pub trait EligibilitySweep {
    type Error;

    fn find_sessions(
        &mut self,
    ) -> impl Future<Output = Result<EligibilitySweepBatch, Self::Error>> + Send;
}

pub struct EligibilitySweepBatch { /* private */ }
impl EligibilitySweepBatch {
    pub fn new(sessions: Vec<SessionId>, continuation: bool) -> Self;
    pub fn with_dispatch_starts(
        sessions: Vec<SessionId>,
        dispatch_starts: HashSet<SessionId>,
        continuation: bool,
    ) -> Self;
    #[must_use]
    pub fn with_unmonitored(self, unmonitored: HashSet<SessionId>) -> Self;
    pub fn into_parts(self) -> (Vec<SessionId>, HashSet<SessionId>, bool);
    // accessors: unmonitored()
}

pub trait EligibilityWorkSource {
    type Error;

    fn next(&mut self) -> impl Future<Output = Result<SessionId, Self::Error>> + Send;
    fn take_returned_dispatch_start(&mut self, _session: SessionId) -> bool;
    fn take_returned_unmonitored(&mut self, _session: SessionId) -> bool;
    fn take_pending_dispatch_start(&mut self) -> Option<SessionId>;
    fn next_pending_dispatch_start(
        &mut self,
    ) -> impl Future<Output = Result<SessionId, Self::Error>> + Send;
}

pub trait EligibilityPass {
    type Error;

    fn failure_stage(_error: &Self::Error) -> &'static str;
    fn failure_turn(_error: &Self::Error) -> Option<TurnId>;
    fn occupancy_expiry_handler(&self) -> Option<Arc<dyn SchedulerPassExpiryHandler>>;
    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;
    fn run_dispatch_start(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;
}

pub trait GoalPassDisposition {
    type Error;
    fn reconcile_success(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;
    fn block_execution_failure(
        &self,
        session: SessionId,
        turn: TurnId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;
}

pub enum GoalAwareEligibilityPassError<PassError, GoalError> {
    Pass {
        source: PassError,
        blocking: Option<GoalError>,
    },
    Reconciliation(GoalError),
}
// impl Display + std::error::Error + ClassifyOperatorFailure

pub struct GoalAwareEligibilityPass<Pass, Disposition> { /* private */ }
impl<Pass, Disposition> GoalAwareEligibilityPass<Pass, Disposition> {
    pub const fn new(pass: Pass, disposition: Disposition) -> Self;
    pub fn into_parts(self) -> (Pass, Disposition);
}
// impl EligibilityPass when Pass: EligibilityPass, Disposition: GoalPassDisposition

pub struct InProcessEligibilityNudge { /* private */ }
// Clone; impl EligibilityNudge

pub struct InProcessEligibilityWorkSource<Sweep>
where
    Sweep: EligibilitySweep,
{ /* private */ }
// impl EligibilityWorkSource when Sweep: EligibilitySweep + Send + 'static
impl<Sweep: EligibilitySweep> InProcessEligibilityWorkSource<Sweep> {
    pub fn new(sweep: Sweep) -> (InProcessEligibilityNudge, Self);
    pub fn with_interval(
        sweep: Sweep,
        sweep_interval: ReconciliationSweepInterval,
    ) -> (InProcessEligibilityNudge, Self);
    pub fn with_options(
        sweep: Sweep,
        sweep_interval: Option<ReconciliationSweepInterval>,
        nudge_buffer_capacity: Option<NonZeroUsize>,
    ) -> (InProcessEligibilityNudge, Self);
}

pub enum SchedulerLoopExit {
    Shutdown,
}

pub struct SchedulerPassOccupancyBound(/* private */);
impl SchedulerPassOccupancyBound {
    pub const fn unbounded() -> Self;
    pub fn try_new(bound: Duration) -> Result<Self, InvalidSchedulerPassOccupancyBound>;
    pub const fn get(self) -> Option<Duration>;
}
pub struct InvalidSchedulerPassOccupancyBound;
// impl Display + std::error::Error

pub struct SchedulerOldestInFlightPass { /* private */ }
impl SchedulerOldestInFlightPass {
    pub const fn new(session: SessionId, started_at: Instant) -> Self;
    pub const fn session(self) -> SessionId;
    pub fn age(self) -> Duration;
}
pub trait SchedulerOccupancyObserver: Send + Sync + 'static {
    fn observe(&self, occupancy: usize, oldest: Option<SchedulerOldestInFlightPass>);
}
pub trait SchedulerPassExpiryHandler: Debug + Send + Sync + 'static {
    fn occupancy_expired(&self, session: SessionId);
}

pub struct SchedulerLoop<WorkSource, Pass> { /* private */ }
impl<WorkSource, Pass> SchedulerLoop<WorkSource, Pass> {
    pub const fn new(work_source: WorkSource, pass: Pass) -> Self;
    pub const fn with_max_in_flight(
        work_source: WorkSource,
        pass: Pass,
        max_in_flight_passes: NonZeroUsize,
    ) -> Self;
    pub const fn paused(work_source: WorkSource, pass: Pass) -> Self;
    pub fn with_occupancy_bound(self, bound: SchedulerPassOccupancyBound) -> Self;
    pub fn with_occupancy_observer(
        self,
        observer: Arc<dyn SchedulerOccupancyObserver>,
    ) -> Self;
    pub fn into_parts(self) -> (WorkSource, Pass);
}
impl<WorkSource, Pass> SchedulerLoop<WorkSource, Pass>
where
    WorkSource: EligibilityWorkSource,
    Pass: EligibilityPass + Send,
    WorkSource::Error: ClassifyOperatorFailure,
    Pass::Error: ClassifyOperatorFailure + Send + 'static,
{
    pub async fn run_until<Shutdown>(
        &mut self,
        shutdown: Shutdown,
    ) -> SchedulerLoopExit
    where
        Shutdown: Future<Output = ()> + Send;
}
```

## application: startup_scan

```rust
pub trait StartupScanIdGenerator {
    fn next_failure_entry_id(&mut self) -> SemanticTranscriptEntryId;
    fn next_terminal_frontier_id(&mut self) -> ContextFrontierId;
    fn next_tool_closure_entry_id(&mut self) -> SemanticTranscriptEntryId;
    fn next_tool_closure_frontier_id(&mut self) -> ContextFrontierId;
    fn next_reclassified_turn_id(&mut self, accepted_input: AcceptedInputId) -> TurnId;
}

pub struct UuidV7StartupScanIdGenerator;
// Default; impl StartupScanIdGenerator

pub enum StartupScanSessionOutcome {
    NoActiveTurn,
    Recovered(Box<FailedAcceptedInputTurn>),
    RecoveredModelCall(Box<ModelCallTerminalOutcome>),
    RecoveredContextCompaction {
        call: ModelCallId,
        disposition: ModelCallDisposition,
    },
    RecoveredToolAttempt(Box<ToolAttemptCrashOutcome>),
    ResumableToolBatch { turn: TurnId },
    ResumablePreparedModelCall { turn: TurnId },
    AwaitingRecoveryDecision { turn: TurnId },
}

pub trait StartupScanRepository {
    type Error: ClassifyOperatorFailure;

    fn active_sessions(
        &mut self,
    ) -> impl Future<Output = Result<Box<[SessionId]>, Self::Error>> + Send;
    fn recover<Generator>(
        &mut self,
        session: SessionId,
        identities: AcceptedInputTurnFailureIdentities,
        ids: &mut Generator,
    ) -> impl Future<Output = Result<StartupScanSessionOutcome, Self::Error>> + Send
    where
        Generator: StartupScanIdGenerator + Send;
}

pub struct StartupScanOutcome { /* private */ }
// sealed: StartupScanService::execute
impl StartupScanOutcome {
    // accessors: recovered_turn_count(), awaiting_recovery_decision_sessions()
}

pub struct StartupScanError<RepositoryError> { /* private */ }
// sealed: StartupScanService::execute
// impl Clone, Debug, Eq, PartialEq, Display, Error, ClassifyOperatorFailure
impl<RepositoryError> StartupScanError<RepositoryError> {
    // accessors: session(), repository_error(), into_repository_error()
}

pub struct StartupScanService<Generator, Repository> { /* private */ }
impl<Generator, Repository> StartupScanService<Generator, Repository> {
    pub const fn new(ids: Generator, repository: Repository) -> Self;
    pub fn into_parts(self) -> (Generator, Repository);
}
impl<
    Generator: StartupScanIdGenerator + Send,
    Repository: StartupScanRepository,
> StartupScanService<Generator, Repository>
{
    pub async fn execute(
        &mut self,
    ) -> Result<StartupScanOutcome, StartupScanError<Repository::Error>>;
}
```

## application: submit_input

```rust
pub enum SubmitInputRequestError {
    InvalidCommandId(InvalidDurableCommandId),
    OversizedContent { utf8_byte_length: usize },
}

pub struct SubmitInputRequest { /* private */ }
impl SubmitInputRequest {
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        delivery: DeliveryRequest,
    ) -> Result<Self, SubmitInputRequestError>;
    pub fn try_new_core_interrupt(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        expected_active_turn: TurnId,
        descendant_scope: DescendantTerminationScope,
        configuration: PerInputConfigurationChoices,
    ) -> Result<Self, SubmitInputRequestError>;
    pub fn try_new_with_content_limit(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        delivery: DeliveryRequest,
        max_content_utf8_bytes: Option<usize>,
    ) -> Result<Self, SubmitInputRequestError>;
    // accessors: command_id(), session(), content(), delivery()
}

pub trait SubmitInputIdGenerator {
    fn next_accepted_input_id(&mut self) -> AcceptedInputId;
    fn next_turn_id(&mut self) -> TurnId;
    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId;
    fn next_context_frontier_id(&mut self) -> ContextFrontierId;
    fn next_closure_decision_command_id(&mut self) -> DurableCommandId;
    fn next_closure_turn_attempt_id(&mut self) -> TurnAttemptId;
}

pub struct UuidV7SubmitInputIdGenerator;  // Default; impl SubmitInputIdGenerator

pub enum SubmitInputOutcome {
    Recorded(SubmitInputResult),
    ConflictingReuse { command_id: DurableCommandId },
}

pub trait SubmitInputTransaction {
    type Error;

    fn handle<NextTurn, NextToolCancellation, NextClosureDecision, NextClosureAttempt>(
        &mut self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
        next_tool_cancellation: NextToolCancellation,
        next_closure_decision: NextClosureDecision,
        next_closure_attempt: NextClosureAttempt,
    ) -> impl Future<Output = Result<SubmitInputOutcome, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        NextToolCancellation:
            FnMut(&[ToolRequestId])
                -> (Vec<SemanticTranscriptEntryId>, ContextFrontierId)
                + Send,
        NextClosureDecision: FnMut() -> DurableCommandId + Send,
        NextClosureAttempt: FnMut() -> TurnAttemptId + Send;
}

pub struct SubmitInputService<Generator, Transaction, Nudge> { /* private */ }
impl<Generator, Transaction, Nudge> SubmitInputService<Generator, Transaction, Nudge> {
    pub const fn new(
        ids: Generator,
        transaction: Transaction,
        nudge: Nudge,
        tool_dispatch_gate: InProcessToolDispatchGate,
    ) -> Self;
    pub fn into_parts(
        self,
    ) -> (
        Generator,
        Transaction,
        Nudge,
        InProcessToolDispatchGate,
    );
}
impl<
    Generator: SubmitInputIdGenerator + Send,
    Transaction: SubmitInputTransaction,
    Nudge: EligibilityNudge,
> SubmitInputService<Generator, Transaction, Nudge>
{
    pub async fn execute(
        &mut self,
        request: SubmitInputRequest,
    ) -> Result<SubmitInputOutcome, Transaction::Error>;
}
```

## application: tool_dispatch_gate

```rust
pub struct InProcessToolDispatchGate { /* private */ }
impl InProcessToolDispatchGate {
    pub fn acquire(
        &self,
        turn: TurnId,
    ) -> impl Future<Output = InProcessToolDispatchPermit> + Send;
}

pub struct InProcessToolDispatchPermit { /* private */ }
```

## application: tool_execution_test_support

```rust
// Compiled only under the `test-support` feature.
pub struct PreparedAttemptIdentities {
    pub session: SessionId,
    pub turn: TurnId,
    pub producing_call: ModelCallId,
    pub request: ToolRequestId,
    pub attempt: ToolAttemptId,
    pub issuing_turn_attempt: TurnAttemptId,
    pub frontier: ContextFrontierId,
}

pub struct PreparedAttemptProposal {
    pub name: ToolName,
    pub arguments: NormalizedToolArguments,
    pub effect_class: ToolEffectClass,
    pub approval: PreparedAttemptApproval,
}

pub enum PreparedAttemptApproval {
    PolicyAuto,
    UserConfirmation { command: DurableCommandId },
}

pub fn prepared_single_attempt_batch(
    identities: PreparedAttemptIdentities,
    proposal: PreparedAttemptProposal,
) -> ToolBatch;

pub struct FixtureTransactionFailures<Error> {
    pub domain_rejection: Error,
    pub declined_crash_classification: Error,
}

pub struct FixtureToolExecutionTransaction<Error> { /* private */ }
impl<Error> FixtureToolExecutionTransaction<Error> {
    pub const fn new(
        batch: ToolBatch,
        failures: FixtureTransactionFailures<Error>,
    ) -> Self;
    pub const fn batch(&self) -> &ToolBatch;
}
// impl ToolExecutionTransaction where Error: ClassifyOperatorFailure + Clone + Send

pub struct RecordingToolExecutor<Executor> { /* private */ }
impl<Executor> RecordingToolExecutor<Executor> {
    pub fn new(inner: Executor) -> (Self, RecordedEvidence);
}
// impl ToolExecutor where Executor: ToolExecutor + Send

pub struct RecordedEvidence { /* private */ }
impl RecordedEvidence {
    pub fn take(&self) -> Option<ToolExecutorEvidence>;
}
```

## application: tool_loop_ports

```rust
pub enum ResolvedToolConversationEntry {
    AssistantToolUse {
        source: SemanticTranscriptEntryRef,
        request: ToolRequest,
    },
    ExecutionResult {
        source: SemanticTranscriptEntryRef,
        request: ToolRequest,
        attempt: EndedToolAttempt,
    },
    Denied {
        source: SemanticTranscriptEntryRef,
        request: ToolRequest,
        approval: ToolApprovalResolution,
    },
    Closed {
        source: SemanticTranscriptEntryRef,
        request: ToolRequest,
    },
}
impl ResolvedToolConversationEntry {
    pub const fn source(&self) -> SemanticTranscriptEntryRef;
}

pub trait DecideToolRequestTransaction {
    type Error: ClassifyOperatorFailure;
    fn decide<NextAttempt>(
        &mut self,
        command: DecideToolRequest,
        next_attempt: NextAttempt,
    ) -> impl Future<Output = Result<PreparedDecideToolRequest, Self::Error>> + Send
    where
        NextAttempt: FnMut() -> TurnAttemptId + Send;
}

pub trait OverrideDeniedToolRequestTransaction {
    type Error: ClassifyOperatorFailure;
    fn override_denied(
        &mut self,
        command: OverrideDeniedToolRequest,
    ) -> impl Future<Output = Result<PreparedOverrideDeniedToolRequest, Self::Error>> + Send;
}

pub struct ToolContinuationIdentities { /* private */ }
impl ToolContinuationIdentities {
    pub fn new(
        result_entries: Vec<SemanticTranscriptEntryId>,
        result_frontier: ContextFrontierId,
        call: ModelCallId,
        target_failure: FailedModelCallTurnIdentities,
        steering_frontier: ContextFrontierId,
    ) -> Self;
    // accessors: result_entries(), result_frontier(), call(), target_failure(),
    // steering_frontier()
}

pub struct ToolCrashClosureIdentities { /* private */ }
impl ToolCrashClosureIdentities {
    pub fn new(
        result_entries: Vec<SemanticTranscriptEntryId>,
        result_frontier: ContextFrontierId,
        failure: FailedModelCallTurnIdentities,
    ) -> Self;
    // accessors: result_entries(), result_frontier(), failure()
}

pub enum PrepareToolContinuationOutcome {
    NoWork,
    Checkpointed(ModelCallId),
    TargetUnavailable(Box<FailedModelCallTurn>),
    PoolExhausted(Box<CredentialPoolExhaustedModelCallTurn>),
    ContextCompactionRequired(Box<ContextHeadroomExhaustedModelCallTurn>),
}

pub enum RetainedToolAttemptObservationStatus {
    Pending,
    AlreadyCommitted,
}

pub enum ToolAttemptAuthorizationStatus {
    Prepared(CurrentToolAttempt),
    InFlight(ToolDispatchAuthority),
}

pub enum ToolAttemptAuthorizationOutcome {
    Authorized(Box<ToolDispatchAuthority>),
    PreauthorizationRejected {
        detail: ToolExecutionErrorDetail,
    },
}

pub trait ToolExecutionTransaction {
    type Error: ClassifyOperatorFailure;
    fn load_active_batch(
        &mut self,
        session: SessionId,
        turn: TurnId,
    ) -> impl Future<Output = Result<Option<ToolBatch>, Self::Error>> + Send;
    fn resume_child_wait(
        &mut self,
        session: SessionId,
        turn: TurnId,
        continuation: TurnAttemptId,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
    fn prepare_next_attempt(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        effect_class: ToolEffectClass,
    ) -> impl Future<Output = Result<Option<CurrentToolAttempt>, Self::Error>> + Send;
    fn authorize_attempt(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        preauthorization: ToolPreauthorization,
    ) -> impl Future<Output = Result<ToolAttemptAuthorizationOutcome, Self::Error>> + Send;
    fn reread_ambiguous_authorization(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
    ) -> impl Future<Output = Result<ToolAttemptAuthorizationStatus, Self::Error>> + Send;
    fn commit_preflight_error(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        error: ToolExecutionError,
    ) -> impl Future<Output = Result<EndedToolAttempt, Self::Error>> + Send;
    fn commit_observation(
        &mut self,
        observation: CorrelatedToolAttemptObservation,
    ) -> impl Future<Output = Result<EndedToolAttempt, Self::Error>> + Send;
    fn reread_observation(
        &mut self,
        observation: &CorrelatedToolAttemptObservation,
    ) -> impl Future<Output = Result<RetainedToolAttemptObservationStatus, Self::Error>> + Send;
    fn reread_durable_completion(
        &mut self,
        correlation: ToolAttemptDispatchCorrelation,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
    fn reread_durable_child_wait(
        &mut self,
        wait: CorrelatedDurableChildWait,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
    fn classify_crash_loss<NextTurn>(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        identities: ToolCrashClosureIdentities,
        next_turn: NextTurn,
    ) -> impl Future<Output = Result<ToolAttemptCrashOutcome, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;
    fn prepare_continuation<NextSteering>(
        &mut self,
        session: SessionId,
        turn: TurnId,
        producing_call: ModelCallId,
        identities: ToolContinuationIdentities,
        next_steering: NextSteering,
    ) -> impl Future<Output = Result<PrepareToolContinuationOutcome, Self::Error>> + Send
    where
        NextSteering: FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send;
}
```

## application: turn_liveness

```rust
pub struct AutomaticReconciliationAttempt(/* private */);
impl AutomaticReconciliationAttempt {
    pub const fn first() -> Self;
    pub const fn try_from_u32(value: u32) -> Option<Self>;
    pub const fn get(self) -> u32;
    pub fn retry_backoff(self, base: Duration, cap: Option<Duration>) -> Duration;
    pub const fn next(self) -> Option<Self>;
    pub const fn is_within_budget(self, budget: Option<u32>) -> bool;
}

pub enum AutomaticReconciliationOperation {
    ModelCall(ModelCallId),
    ToolAttempt(ToolAttemptId),
}

pub struct ClaimedAutomaticReconciliation { /* private */ }
impl ClaimedAutomaticReconciliation {
    pub const fn new(
        session: SessionId,
        turn: TurnId,
        operation: AutomaticReconciliationOperation,
        attempt: AutomaticReconciliationAttempt,
    ) -> Self;
    // accessors: session(), turn(), operation(), attempt()
}

pub struct ExhaustedAutomaticReconciliation { /* private */ }
impl ExhaustedAutomaticReconciliation {
    pub const fn new(
        session: SessionId,
        turn: TurnId,
        operation: AutomaticReconciliationOperation,
    ) -> Self;
    // accessors: session(), turn(), operation()
}

pub struct AutomaticReconciliationBatch { /* private */ }
impl AutomaticReconciliationBatch {
    pub fn new(
        claimed: Box<[ClaimedAutomaticReconciliation]>,
        exhausted: Box<[ExhaustedAutomaticReconciliation]>,
    ) -> Self;
    pub fn claimed(&self) -> &[ClaimedAutomaticReconciliation];
    pub fn exhausted(&self) -> &[ExhaustedAutomaticReconciliation];
}

pub enum AutomaticReconciliationFailureKind {
    Infrastructure,
    Integrity,
}
impl AutomaticReconciliationFailureKind {
    pub const fn as_str(self) -> &'static str;
}

pub enum AutomaticReconciliationOutcome {
    Reconciled,
    Superseded,
}

pub struct StaleActiveTurnBound(/* private */);
impl StaleActiveTurnBound {
    pub fn try_new(bound: Duration) -> Result<Self, TurnLivenessBoundError>;
    pub const fn as_secs(self) -> u64;
    pub const fn get(self) -> Duration;
}

pub struct TurnLivenessScanInterval(/* private */);
impl TurnLivenessScanInterval {
    pub fn try_new(interval: Duration) -> Result<Self, TurnLivenessBoundError>;
    pub const fn get(self) -> Duration;
}

pub enum TurnLivenessBoundError {
    Zero,
    Subsecond,
    TimerRange,
}
// impl Display + std::error::Error

pub struct TurnLivenessEvidence { /* private */ }
impl TurnLivenessEvidence {
    pub const fn new(current_attempt: TurnAttemptId, outbox_frontier: Option<u64>) -> Self;
    pub const fn current_attempt(self) -> TurnAttemptId;
    pub const fn outbox_frontier(self) -> Option<u64>;
}

pub struct StaleTurnCandidate { /* private */ }
impl StaleTurnCandidate {
    pub const fn new(
        session: SessionId,
        turn: TurnId,
        evidence: TurnLivenessEvidence,
    ) -> Self;
    pub const fn session(self) -> SessionId;
    pub const fn turn(self) -> TurnId;
    pub const fn evidence(self) -> TurnLivenessEvidence;
}

pub enum StaleTurnOutcome {
    Terminalized,
    Superseded,
}

pub enum TurnLivenessGuardKind {
    Quiescent,
    SlotHeld,
}
impl TurnLivenessGuardKind {
    pub const fn as_str(self) -> &'static str;
}

pub struct DurableTurnLivenessObservation { /* private */ }
impl DurableTurnLivenessObservation {
    pub const fn new(candidate: StaleTurnCandidate, ordinal: NonZeroU64) -> Self;
    pub const fn candidate(self) -> StaleTurnCandidate;
    pub const fn ordinal(self) -> NonZeroU64;
}

pub struct TurnLivenessLedger { /* private */ }
impl TurnLivenessLedger {
    pub const fn new(
        bound: StaleActiveTurnBound,
        scan_interval: TurnLivenessScanInterval,
    ) -> Self;
    pub const fn bound(&self) -> StaleActiveTurnBound;
    pub const fn scan_interval(&self) -> TurnLivenessScanInterval;
    pub fn reconcile(
        self,
        observations: &[DurableTurnLivenessObservation],
    ) -> Box<[StaleTurnCandidate]>;
}
```

## domain: runner

```rust
pub enum RunnerDomainError {
    Empty,
    ContainsNull,
    TooLong,
    InvalidName,
    InvalidHex,
    InvalidBranchName,
    InvalidRelativePath,
    InvalidToolInputSchema,
    DuplicateCapabilityClass(RunnerCapabilityClass),
    DuplicateTool(ToolName),
    DuplicateProfile(CredentialProfileName),
    DuplicateWorkspaceCapability(WorkspaceCapability),
    DuplicateSandboxProfile(RunnerSandboxProfile),
    TooManyPermissionOverrides,
    TooManyAdvertisedRepositories,
    UndeclaredProfileTool(ToolName),
    UnsupportedDaemonIdempotency(ToolName),
    EnrollmentRevoked,
    CapabilityClassNotAllowed(RunnerCapabilityClass),
    ToolUndeclared(ToolName),
    ToolLocusNotAllowed(ToolName),
    CredentialProfileUndeclared(CredentialProfileName),
    WorkspaceCapabilityNotAllowed(WorkspaceCapability),
    SandboxProfileNotAllowed(RunnerSandboxProfile),
    RepositoryProfileUnavailable(CredentialProfileName),
    InvalidState,
    CorrelationMismatch,
    GenerationExhausted,
    AttemptIdentityReuse,
    SelectorMismatch,
    CredentialProfileUnavailable,
    WorkingDirectoryMismatch,
    WorkspaceCapabilityUnavailable,
    SandboxProfileUnavailable,
    RepositoryUnavailable,
    WorkspaceMismatch,
    ToolUnavailable,
    GrantRevoked,
    RegistrationChanged,
    RegistrationInProgress,
    CorruptStoredFacts,
}

pub struct RunnerCapabilityClass(/* private */);
pub struct CredentialProfileName(/* private */);
pub struct RunnerWorkingDirectory(/* private */);
impl RunnerWorkingDirectory {
    pub const MAX_BYTES: usize;
}
pub struct WorkspaceRepositoryKey(/* private */);
pub struct CanonicalCloneUrlDigest(/* private */);
pub struct WorkspaceRevision(/* private */);
pub struct WorkspaceBranchName(/* private */);
pub struct WorkspaceRelativePath(/* private */);
impl RunnerCapabilityClass {
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError>;
    // accessor: as_str()
}
impl CredentialProfileName {
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError>;
    // accessor: as_str()
}
impl RunnerWorkingDirectory {
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError>;
    // accessor: as_str()
}
impl WorkspaceRepositoryKey {
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError>;
    // accessor: as_str()
}
impl CanonicalCloneUrlDigest {
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError>;
    // accessor: as_str()
}
impl WorkspaceRevision {
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError>;
    // accessor: as_str()
}
impl WorkspaceBranchName {
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError>;
    // accessor: as_str()
}
impl WorkspaceRelativePath {
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError>;
    // accessor: as_str()
}

pub enum RunnerSelector {
    Identity(RunnerId),
    CapabilityClass(RunnerCapabilityClass),
}

pub enum ToolAdmissibleLoci {
    DaemonOnly,
    RunnerOnly { selector: RunnerSelector },
    DaemonOrRunner { selector: RunnerSelector },
}
// accessors: allows_daemon(), runner_selector()

pub enum RunnerToolEffectClass {
    Pure,
    Idempotent,
    SideEffecting,
}

pub struct RunnerToolDeclaration { /* private */ }
impl RunnerToolDeclaration {
    pub const fn new(
        name: ToolName,
        model: RunnerToolModelDefinition,
        permission: ToolPermissionDefault,
        effect: RunnerToolEffectClass,
        loci: ToolAdmissibleLoci,
    ) -> Self;
    // accessors: name(), model(), permission(), effect(), loci()
}
pub struct RunnerToolModelDefinition { /* private */ }
impl RunnerToolModelDefinition {
    pub fn try_new(
        description: String,
        input_schema: String,
    ) -> Result<Self, RunnerDomainError>;
    // accessors: description(), input_schema()
}

pub enum CredentialToolApproval {
    Automatic,
    SessionPolicy,
}

pub struct CredentialProfilePolicy { /* private */ }
impl CredentialProfilePolicy {
    pub fn try_new(
        name: CredentialProfileName,
        approvals: impl IntoIterator<Item = (ToolName, CredentialToolApproval)>,
    ) -> Result<Self, RunnerDomainError>;
    // accessors: name(), approval_for()
    pub fn approvals(&self) -> impl Iterator<Item = (&ToolName, CredentialToolApproval)>;
}
pub enum WorkspaceRecovery {
    Commit { revision: WorkspaceRevision },
    Branch {
        name: WorkspaceBranchName,
        revision: WorkspaceRevision,
    },
}

pub enum RunnerSandboxProfile {
    Ambient,
    WorkspaceRestricted,
}
pub enum RunnerToolPermissionOverride {
    Auto,
    Confirm,
}
pub struct RunnerToolPermissionOverrides(/* private */);
impl RunnerToolPermissionOverrides {
    pub fn try_new(
        overrides: impl IntoIterator<Item = (ToolName, RunnerToolPermissionOverride)>,
    ) -> Result<Self, RunnerDomainError>;
    pub fn get(&self, tool: &ToolName) -> Option<RunnerToolPermissionOverride>;
    pub fn iter(
        &self,
    ) -> impl Iterator<Item = (&ToolName, RunnerToolPermissionOverride)>;
}
pub struct RunnerRepositoryEntry { /* private */ }
impl RunnerRepositoryEntry {
    pub const fn new(
        key: WorkspaceRepositoryKey,
        credential_profile: Option<CredentialProfileName>,
    ) -> Self;
    pub const fn key(&self) -> &WorkspaceRepositoryKey;
    pub const fn credential_profile(&self) -> Option<&CredentialProfileName>;
}

pub enum WorkspaceCapability {
    WorktreePerSession,
}
pub struct RunnerCatalog { /* private */ }
impl RunnerCatalog {
    pub fn try_new(
        classes: impl IntoIterator<Item = RunnerCapabilityClass>,
        tools: impl IntoIterator<Item = RunnerToolDeclaration>,
        profiles: impl IntoIterator<Item = CredentialProfilePolicy>,
        workspaces: impl IntoIterator<Item = WorkspaceCapability>,
        sandboxes: impl IntoIterator<Item = RunnerSandboxProfile>,
    ) -> Result<Self, RunnerDomainError>;
}
pub struct RunnerAdvertisement { /* private */ }
impl RunnerAdvertisement {
    pub const MAX_REPOSITORIES: usize;
    pub fn new(
        classes: impl IntoIterator<Item = RunnerCapabilityClass>,
        tools: impl IntoIterator<Item = ToolName>,
        profiles: impl IntoIterator<Item = CredentialProfileName>,
        workspaces: impl IntoIterator<Item = WorkspaceCapability>,
        sandboxes: impl IntoIterator<Item = RunnerSandboxProfile>,
        repositories: impl IntoIterator<Item = RunnerRepositoryEntry>,
    ) -> Self;
    pub fn classes(&self) -> impl Iterator<Item = &RunnerCapabilityClass>;
    pub fn tools(&self) -> impl Iterator<Item = &ToolName>;
    pub fn profiles(&self) -> impl Iterator<Item = &CredentialProfileName>;
    pub fn workspaces(&self) -> impl Iterator<Item = WorkspaceCapability> + '_;
    pub fn sandboxes(&self) -> impl Iterator<Item = RunnerSandboxProfile> + '_;
    pub fn repositories(&self) -> impl Iterator<Item = &RunnerRepositoryEntry>;
}

pub enum RunnerEnrollmentState {
    Active,
    Revoked,
}
pub struct RunnerEnrollment { /* private */ }
impl RunnerEnrollment {
    pub fn new(
        enrollment: RunnerEnrollmentId,
        runner: RunnerId,
        authentication: RunnerAuthenticationId,
        allowed_classes: impl IntoIterator<Item = RunnerCapabilityClass>,
    ) -> Self;
    pub fn revoke(self) -> Result<Self, RunnerDomainError>;
    pub fn revoke_in_place(&mut self) -> Result<(), RunnerDomainError>;
    pub fn register(
        &self,
        advertisement: RunnerAdvertisement,
        catalog: &RunnerCatalog,
    ) -> Result<ValidatedRunnerRegistration, RunnerDomainError>;
    pub fn prepare_registration(
        &self,
        advertisement: RunnerAdvertisement,
        catalog: &RunnerCatalog,
    ) -> Result<PreparedRunnerRegistration, RunnerDomainError>;
    pub fn reconstitute(
        input: RunnerEnrollmentReconstitutionInput,
    ) -> Result<Self, RunnerDomainError>;
    // accessors: enrollment(), runner(), authentication(), state(),
    // allowed_classes(), last_issued_registration_revision()
}
pub struct RunnerEnrollmentReconstitutionInput {
    /* public complete typed facts, including independently recorded optional last registration revision */
}
pub struct PreparedRunnerRegistration { /* private */ }
impl PreparedRunnerRegistration {
    pub const fn registration(&self) -> &ValidatedRunnerRegistration;
    pub fn commit(self) -> Result<ValidatedRunnerRegistration, RunnerDomainError>;
}
pub struct ValidatedRunnerRegistration { /* private */ }
pub struct ValidatedRunnerRegistrationReconstitutionInput {
    /* public complete typed catalog-validation facts, including exact revision,
       sandbox inventory, and repository-entry inventory */
}
impl ValidatedRunnerRegistration {
    // accessors: enrollment(), runner(), authentication(), revision()
    pub fn satisfies(&self, selector: &RunnerSelector) -> bool;
    pub fn tool(&self, tool: &ToolName) -> Option<&RunnerToolDeclaration>;
    pub fn profile(
        &self,
        profile: &CredentialProfileName,
    ) -> Option<&CredentialProfilePolicy>;
    pub fn supports_workspace(&self, capability: WorkspaceCapability) -> bool;
    pub fn supports_sandbox(&self, profile: RunnerSandboxProfile) -> bool;
    pub fn repository(
        &self,
        key: &WorkspaceRepositoryKey,
    ) -> Option<&RunnerRepositoryEntry>;
    pub fn tool_names(&self) -> impl Iterator<Item = &ToolName>;
    pub fn classes(&self) -> impl Iterator<Item = &RunnerCapabilityClass>;
    pub fn tools(&self) -> impl Iterator<Item = &RunnerToolDeclaration>;
    pub fn profiles(&self) -> impl Iterator<Item = &CredentialProfilePolicy>;
    pub fn workspaces(&self) -> impl Iterator<Item = WorkspaceCapability> + '_;
    pub fn sandboxes(&self) -> impl Iterator<Item = RunnerSandboxProfile> + '_;
    pub fn repositories(&self) -> impl Iterator<Item = &RunnerRepositoryEntry>;
    pub fn reconstitute(
        enrollment: &RunnerEnrollment,
        catalog: &RunnerCatalog,
        input: ValidatedRunnerRegistrationReconstitutionInput,
    ) -> Result<Self, RunnerDomainError>;
}

pub struct RunnerGeneration(/* private NonZeroU64 */);
impl RunnerGeneration {
    pub const fn one() -> Self;
    pub const fn try_from_u64(value: u64) -> Option<Self>;
    pub const fn get(self) -> u64;
    pub const fn checked_next(self) -> Option<Self>;
}
pub struct RunnerLeaseCorrelation {
    /* public exact fence fields */
}
pub struct RunnerLeaseOfferRequest {
    /* public exact initial-offer fields */
}
pub struct RunnerToolAttemptAuthorization { /* private */ }
impl RunnerToolAttemptAuthorization {
    pub const fn tool(&self) -> &ToolName;
}
pub enum RunnerLeaseState {
    Offered,
    Claimed,
    Completed,
    LostUnclaimed,
    LostExecutionPossible,
    LostClaimed,
}
pub struct RunnerLeaseNoExecutionProof { /* private durable correlation */ }
impl RunnerLeaseNoExecutionProof {
    pub const fn correlation(&self) -> &RunnerLeaseCorrelation;
}
pub struct RunnerLease { /* private */ }
impl RunnerLease {
    // accessors: correlation(), state(), generation(), attempt(), tool(),
    //   credential_authorization(), session(), runner(), effect()
    pub fn claim(
        self,
        correlation: RunnerLeaseCorrelation,
    ) -> Result<Self, RunnerDomainError>;
    pub fn complete(
        self,
        correlation: RunnerLeaseCorrelation,
    ) -> Result<Self, RunnerDomainError>;
    pub fn lose(self) -> Result<RunnerLeaseLoss, RunnerDomainError>;
    pub fn lose_unclaimed(
        self,
        proof: &RunnerLeaseNoExecutionProof,
    ) -> Result<RunnerLeaseLoss, RunnerDomainError>;
    pub fn reconstitute(
        input: RunnerLeaseReconstitutionInput,
        registration: &ValidatedRunnerRegistration,
    ) -> Result<Self, RunnerDomainError>;
    pub fn reconstitute_loss(
        input: RunnerLeaseReconstitutionInput,
        registration: &ValidatedRunnerRegistration,
        no_execution: Option<RunnerLeaseCorrelation>,
    ) -> Result<RunnerLeaseLoss, RunnerDomainError>;
    pub fn into_reconstituted_loss(
        self,
        no_execution: Option<RunnerLeaseCorrelation>,
        retry_preparation: RunnerLeaseRetryPreparation,
    ) -> Result<RunnerLeaseLoss, RunnerDomainError>;
}
pub enum RunnerLeaseRetryPreparation {
    Available,
    Prepared,
}
pub struct RunnerLeaseReconstitutionInput {
    /* public raw lease projection, independent fence, and retry preparation */
}
pub struct RunnerLeaseLoss { /* private, produced only by checked RunnerLease transitions */ }
impl RunnerLeaseLoss {
    // accessors: lost(), retry(), crash_attempt(), no_execution_proof()
}
pub struct RunnerLeaseRetryAuthority { /* private */ }
impl RunnerLeaseRetryAuthority {
    // accessors: generation()
    pub fn prepare_unclaimed_attempt(
        &self,
        batch: ToolBatch,
    ) -> Result<RunnerUnclaimedAttemptReauthorization, RunnerDomainError>;
    pub fn prepare_claimed_attempt(
        &self,
        batch: ToolBatch,
        attempt: ToolAttemptId,
    ) -> Result<RunnerClaimedAttemptReplacement, RunnerDomainError>;
}
pub struct RunnerUnclaimedAttemptReauthorization { /* private */ }
impl RunnerUnclaimedAttemptReauthorization {
    // accessor: batch()
    pub fn into_parts(self) -> (ToolBatch, RunnerToolAttemptAuthorization);
}

pub struct RunnerClaimedAttemptReplacement { /* private */ }
impl RunnerClaimedAttemptReplacement {
    // accessors: batch(), retired(), source(), replacement()
    pub fn into_parts(
        self,
    ) -> (
        ToolBatch,
        EndedToolAttempt,
        RunnerToolAttemptAuthorization,
    );
}

pub enum WorkingDirectorySelection {
    RunnerDefault,
    Exact(RunnerWorkingDirectory),
}
pub enum WorkspaceRequirement {
    None,
    RepositoryWorktree {
        repository: WorkspaceRepositoryKey,
    },
}
pub struct ProvisionedWorkspace {
    pub session: SessionId,
    pub placement_revision: RunnerGeneration,
    pub runner: RunnerId,
    pub repository: Option<WorkspaceRepositoryKey>,
    pub canonical_clone_url_digest: Option<CanonicalCloneUrlDigest>,
    pub credential_profile: Option<CredentialProfileName>,
    pub sandbox: RunnerSandboxProfile,
    pub working_directory: RunnerWorkingDirectory,
    pub relative_path: WorkspaceRelativePath,
    pub manifest_id: WorkspaceManifestId,
    pub recovery: Option<WorkspaceRecovery>,
}
pub struct SessionRunnerPlacementRequest {
    pub selector: RunnerSelector,
    pub working_directory: WorkingDirectorySelection,
    pub credential_profile: Option<CredentialProfileName>,
    pub workspace: WorkspaceRequirement,
    pub sandbox: RunnerSandboxProfile,
    pub permission_overrides: RunnerToolPermissionOverrides,
}
pub struct RunnerCredentialGrantLineage {
    /* public exact last-grant runner and revision */
}
pub struct PinnedRunnerPlacement {
    /* public complete pinned facts, including optional grant lineage, sandbox,
       permission overrides, and provisioned-workspace recovery facts */
}
pub enum RunnerPlacementLossSource {
    Connection,
    Registration,
}
pub struct RunnerLostBeforePin { /* private exact runner */ }
impl RunnerLostBeforePin {
    pub const fn from_stored(runner: RunnerId) -> Self;
    // accessor: runner()
}
pub struct LostPinnedRunnerPlacement { /* private pinned facts + loss source */ }
impl LostPinnedRunnerPlacement {
    pub const fn from_stored(
        pinned: PinnedRunnerPlacement,
        source: RunnerPlacementLossSource,
    ) -> Self;
    // accessors: pinned(), source()
}
pub enum AbandonedRunnerPlacement {
    BeforePin(RunnerLostBeforePin),
    Pinned(Box<LostPinnedRunnerPlacement>),
}
pub enum SessionRunnerPlacementState {
    Unpinned,
    RunnerLostBeforePin(RunnerLostBeforePin),
    Pinned(PinnedRunnerPlacement),
    RunnerLost(LostPinnedRunnerPlacement),
    RunnerAbandoned(AbandonedRunnerPlacement),
}
pub struct SessionRunnerPlacement { /* private */ }
pub enum RunnerPlacementReconstitutionHistory {
    Initial,
    PrePinReplacements(Vec<RunnerPrePinReplacementHistory>),
}
pub struct RunnerPrePinReplacementHistory {
    pub prior_revision: RunnerGeneration,
    pub lost_runner: RunnerId,
    pub prior_request: SessionRunnerPlacementRequest,
    pub replacement_request: SessionRunnerPlacementRequest,
}
pub struct SessionRunnerPlacementReconstitutionInput {
    /* public complete typed placement facts + append-only history proof */
}
impl SessionRunnerPlacement {
    // placement is the only producer of initial and retry lease offers
    pub const fn new(
        session: SessionId,
        request: SessionRunnerPlacementRequest,
    ) -> Self;
    pub fn pin_and_offer_lease(
        self,
        enrollment: &RunnerEnrollment,
        registration: &ValidatedRunnerRegistration,
        directory: RunnerWorkingDirectory,
        workspace: Option<ProvisionedWorkspace>,
        authorization: RunnerToolAttemptAuthorization,
        offer: RunnerLeaseOfferRequest,
    ) -> Result<SessionRunnerPin, RunnerDomainError>;
    pub fn offer_lease(
        &self,
        enrollment: &RunnerEnrollment,
        registration: &ValidatedRunnerRegistration,
        grant: Option<&CredentialProfileGrant>,
        authorization: RunnerToolAttemptAuthorization,
        offer: RunnerLeaseOfferRequest,
    ) -> Result<RunnerLease, RunnerDomainError>;
    pub fn offer_retry(
        &self,
        enrollment: &RunnerEnrollment,
        registration: &ValidatedRunnerRegistration,
        grant: Option<&CredentialProfileGrant>,
        loss: RunnerLeaseLoss,
        authorization: RunnerToolAttemptAuthorization,
    ) -> Result<RunnerLease, RunnerDomainError>;
    pub fn mark_runner_lost(self) -> Result<Self, RunnerDomainError>;
    pub fn mark_runner_lost_before_pin(
        self,
        runner: RunnerId,
    ) -> Result<Self, RunnerDomainError>;
    pub fn reconcile_registration(
        self,
        registration: &ValidatedRunnerRegistration,
    ) -> Result<Self, RunnerDomainError>;
    pub fn replace_lost_runner_before_pin(
        self,
        request: SessionRunnerPlacementRequest,
        registration: &ValidatedRunnerRegistration,
    ) -> Result<RunnerPrePinReplacement, RunnerDomainError>;
    pub fn replace_lost_runner(
        self,
        request: SessionRunnerPlacementRequest,
        registration: &ValidatedRunnerRegistration,
        directory: RunnerWorkingDirectory,
        workspace: Option<ProvisionedWorkspace>,
        prior_grant: Option<CredentialProfileGrant>,
    ) -> Result<RunnerPlacementReplacement, RunnerDomainError>;
    pub fn abandon_lost_runner(self) -> Result<Self, RunnerDomainError>;
    pub fn replace_credential_profile(
        self,
        grant: CredentialProfileGrant,
        registration: &ValidatedRunnerRegistration,
        profile: CredentialProfileName,
        tools: impl IntoIterator<Item = ToolName>,
    ) -> Result<CredentialProfilePlacementReplacement, RunnerDomainError>;
    pub fn reconstitute(
        input: SessionRunnerPlacementReconstitutionInput,
        expected_session: SessionId,
        registration: Option<&ValidatedRunnerRegistration>,
        profileless_tombstone: Option<&CredentialProfileGrant>,
    ) -> Result<Self, RunnerDomainError>;
    // accessors: session(), request(), state(), revision()
}
pub struct SessionRunnerPin {
    /* public placement, optional initial grant, and initial lease */
}
pub struct RunnerPrePinReplacement {
    /* public successor placement, exact loss, and before/after requests */
}
pub struct RunnerPlacementReplacement {
    /* public placement, change, optional replacement grant, and optional complete grant change */
}
pub struct RunnerPlacementChange {
    /* public before-and-after request and pinned facts */
}
pub struct CredentialProfilePlacementReplacement {
    /* public placement, placement change, and grant replacement */
}

pub enum CredentialProfileGrantState {
    Active,
    Revoked,
}
pub struct CredentialProfileGrant { /* private */ }
pub struct CredentialProfileGrantReconstitutionInput {
    /* public complete typed grant and approval facts */
}
impl CredentialProfileGrant {
    // accessors: state(), revision(), lineage(), profile(), session(), runner()
    pub fn tools(&self) -> impl Iterator<Item = &ToolName>;
    pub fn approvals(&self) -> impl Iterator<Item = (&ToolName, CredentialToolApproval)>;
    pub fn revoke(self) -> Result<Self, RunnerDomainError>;
    pub fn reconstitute(
        input: CredentialProfileGrantReconstitutionInput,
        expected_session: SessionId,
        registration: &ValidatedRunnerRegistration,
        sandbox: RunnerSandboxProfile,
        permission_overrides: &RunnerToolPermissionOverrides,
    ) -> Result<Self, RunnerDomainError>;
}
pub struct RunnerCredentialGrantChange {
    /* public optional complete before-and-after grant facts */
}
pub struct CredentialDispatchAuthorization {
    /* public exact tool/profile decision facts; produced only inside a lease */
}
pub struct CredentialProfileGrantReplacement {
    /* public grant and change */
}
pub struct CredentialProfileChange {
    /* public before-and-after facts */
}
```

## domain: goal

```rust
pub struct GoalStatement(/* private String */);
pub struct GoalNeed(/* private String */);
pub struct GoalGuidance(/* private String */);
pub struct GoalReport(/* private String */);
pub struct FinishConditionStatement(/* private String */);
impl GoalStatement {
    pub fn try_new(value: String) -> Result<Self, GoalTextError>;
    // accessors: as_str(), into_string()
}
impl GoalNeed {
    pub fn try_new(value: String) -> Result<Self, GoalTextError>;
    // accessors: as_str(), into_string()
}
impl GoalGuidance {
    pub fn try_new(value: String) -> Result<Self, GoalTextError>;
    // accessors: as_str(), into_string()
}
impl GoalReport {
    pub fn try_new(value: String) -> Result<Self, GoalTextError>;
    // accessors: as_str(), into_string()
}
impl FinishConditionStatement {
    pub fn try_new(value: String) -> Result<Self, GoalTextError>;
    // accessors: as_str(), into_string()
}

pub enum GoalTextError {
    Empty,
    ContainsNull,
    Oversized { utf8_byte_length: usize },
}

pub struct GoalGeneration(/* private NonZeroU64 */);
impl GoalGeneration {
    pub const fn new(value: NonZeroU64) -> Self;
    // accessor: get()
}
pub struct GoalEventOrdinal(/* private NonZeroU64 */);
impl GoalEventOrdinal {
    pub const fn new(value: NonZeroU64) -> Self;
    // accessor: get()
}

pub enum GoalTurnSource {
    UserEvent(GoalEventOrdinal),
    SuccessfulTurn(TurnId),
}

pub struct GoalUserProvenance(/* private DurableCommandId */);
impl GoalUserProvenance {
    pub const fn new(command: DurableCommandId) -> Self;
    // accessor: command()
}
pub struct GoalModelProvenance { /* private turn + tool request */ }
impl GoalModelProvenance {
    pub const fn new(turn: TurnId, tool_request: ToolRequestId) -> Self;
    // accessors: turn(), tool_request(), report_ref()
}
pub struct GoalSchedulerProvenance(/* private TurnId */);
impl GoalSchedulerProvenance {
    pub const fn new(turn: TurnId) -> Self;
    // accessor: turn()
}
pub struct GoalReportRef { /* private turn + tool request */ }
impl GoalReportRef {
    // accessors: turn(), tool_request()
}

pub enum GoalModelBlockedReasonKind {
    UserInputRequired,
    ExternalChangeRequired,
    AuthorizationRequired,
}
pub enum GoalBlockedReasonKind {
    UserInputRequired,
    ExternalChangeRequired,
    AuthorizationRequired,
    ExecutionFailure,
    FinishCheckFailed,
}
pub enum GoalBlockProvenance {
    Model { reason: GoalModelBlockedReasonKind, provenance: GoalModelProvenance },
    ExecutionFailure { provenance: GoalSchedulerProvenance },
    FinishCheck { provenance: GoalModelProvenance },
}
impl GoalBlockProvenance {
    // accessor: reason_kind()
}

pub enum GoalState {
    Pursuing,
    Blocked { reason: GoalBlockedReasonKind, need: GoalNeed },
    Achieved { report: GoalReportRef },
    UserStopped,
    Superseded { by_generation: GoalGeneration },
    SessionClosed { outcome: SessionClosureOutcome },
}
impl GoalState {
    pub const fn is_open(&self) -> bool;
}
pub struct GoalGenerationSnapshot { /* private generation + statement + state */ }
impl GoalGenerationSnapshot {
    // accessors: generation(), statement(), state()
}

pub struct GoalEvent { /* private ordinal + generation + kind */ }
impl GoalEvent {
    pub const fn from_stored_parts(
        ordinal: GoalEventOrdinal,
        generation: GoalGeneration,
        kind: GoalEventKind,
    ) -> Self;
    // accessors: ordinal(), generation(), kind()
}
pub enum GoalEventKind {
    Commissioned { statement: GoalStatement, provenance: GoalUserProvenance },
    Blocked { block: GoalBlockProvenance, need: GoalNeed },
    Resumed { guidance: Option<GoalGuidance>, provenance: GoalUserProvenance },
    Achieved { report: GoalReport, provenance: GoalModelProvenance },
    UserStopped { provenance: GoalUserProvenance },
    Superseded {
        replacement_statement: GoalStatement,
        provenance: GoalUserProvenance,
    },
    SessionClosed { outcome: SessionClosureOutcome, provenance: LifecycleActor },
}

pub struct Goal { /* private session + generations + events */ }
impl Goal {
    pub fn commission(
        session: SessionId,
        statement: GoalStatement,
        provenance: GoalUserProvenance,
    ) -> Self;
    pub fn commission_successor(
        self,
        statement: GoalStatement,
        provenance: GoalUserProvenance,
    ) -> Result<Self, GoalTransitionError>;
    pub fn declare_blocked(
        self,
        reason: GoalModelBlockedReasonKind,
        need: GoalNeed,
        provenance: GoalModelProvenance,
    ) -> Result<Self, GoalTransitionError>;
    pub fn block_execution_failure(
        self,
        need: GoalNeed,
        provenance: GoalSchedulerProvenance,
    ) -> Result<Self, GoalTransitionError>;
    pub fn block_finish_check(
        self,
        need: GoalNeed,
        provenance: GoalModelProvenance,
    ) -> Result<Self, GoalTransitionError>;
    pub fn resume(
        self,
        guidance: Option<GoalGuidance>,
        provenance: GoalUserProvenance,
    ) -> Result<Self, GoalTransitionError>;
    pub fn declare_achieved(
        self,
        report: GoalReport,
        provenance: GoalModelProvenance,
    ) -> Result<Self, GoalTransitionError>;
    pub fn stop(self, provenance: GoalUserProvenance) -> Result<Self, GoalTransitionError>;
    pub fn close_with_session(
        self,
        outcome: SessionClosureOutcome,
        provenance: LifecycleActor,
    ) -> Result<Self, GoalTransitionError>;
    pub fn supersede(
        self,
        replacement_statement: GoalStatement,
        provenance: GoalUserProvenance,
    ) -> Result<Self, GoalTransitionError>;
    // accessors: session(), generations(), current(), events()
}

pub enum GoalTransitionFailure {
    RequiresPursuing,
    RequiresBlocked,
    RequiresPursuingOrBlocked,
    RequiresNoActiveGoal,
    GenerationExhausted,
    EventOrdinalExhausted,
}
pub struct GoalTransitionError { /* unchanged goal + failure */ }
impl GoalTransitionError {
    // accessors: failure(), goal(), into_goal()
}

pub struct GoalReconstitutionInput { /* private session + complete ordered events */ }
impl GoalReconstitutionInput {
    pub fn new(session: SessionId, events: Vec<GoalEvent>) -> Self;
    pub fn reconstitute(self) -> Result<Goal, GoalReconstitutionError>;
}
pub enum GoalReconstitutionFailure {
    MissingCommission,
    EventSequence,
    InvalidTransition,
}
pub struct GoalReconstitutionError(/* private GoalReconstitutionFailure */);
impl GoalReconstitutionError {
    // accessor: failure()
}
```

## domain: goal_command

```rust
pub enum GoalUserAction {
    Attach(GoalStatement),
    Resume(Option<GoalGuidance>),
    Stop {
        descendant_scope: DescendantTerminationScope,
    },
    Supersede(GoalStatement),
}
impl GoalUserAction {
    pub const fn starts_pursuit(&self) -> bool;
}
pub struct GoalUserCommand { /* private command identity + session + action */ }
impl GoalUserCommand {
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        action: GoalUserAction,
    ) -> Self;
    // accessors: command_id(), session(), action()
}

pub enum GoalCommandRejection {
    SessionNotFound,
    SessionClosing,
    GoalAlreadyAttached,
    GoalNotAttached,
    UnknownModelAlias,
    AcceptancePositionExhausted,
    RequiresBlocked,
    RequiresPursuingOrBlocked,
    GenerationExhausted,
    EventOrdinalExhausted,
}
pub enum GoalCommandResult {
    Applied(GoalEvent),
    Rejected(GoalCommandRejection),
}
pub struct ReconstitutedGoalCommand { /* private command + result */ }
impl ReconstitutedGoalCommand {
    pub const fn new(command: GoalUserCommand, result: GoalCommandResult) -> Self;
    // accessors: command(), result()
}
```

## domain: repo_watch

```rust
pub enum RepoWatchTextError {
    Empty,
    ContainsNull,
    TooLong { bytes: usize, maximum: usize },
    TooManyCharacters { characters: usize, maximum: usize },
    Malformed,
    UnanchoredPattern,
    InvalidPattern { reason: String },
}
// implements Error.

pub struct RepositorySlug(/* private String */);
impl RepositorySlug {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), into_string()
}

pub struct BranchName(/* private String */);
pub struct LabelName(/* private String */);
pub struct RepoWatchAuthorLogin(/* private String */);
pub struct CheckRunName(/* private String */);
pub struct WorkflowName(/* private String */);
pub struct ReactionContent(/* private String */);
pub struct RepoWatchRuleId(/* private String */);
pub struct ReviewThreadId(/* private String */);
pub struct PullRequestTitle(/* private String */);
impl BranchName {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), into_string()
}
impl LabelName {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), into_string()
}
impl RepoWatchAuthorLogin {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), into_string()
}
impl CheckRunName {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), into_string()
}
impl WorkflowName {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), into_string()
}
impl ReactionContent {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), into_string()
}
impl RepoWatchRuleId {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), into_string()
}
impl ReviewThreadId {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), into_string()
}
impl PullRequestTitle {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), into_string()
}

pub struct PullRequestBody(/* private String */);
impl PullRequestBody {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), into_string()
}

pub struct CommitSha(/* private String */);
impl CommitSha {
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), into_string()
}

pub struct PullRequestNumber(/* private NonZeroU64 */);
impl PullRequestNumber {
    pub const fn new(value: NonZeroU64) -> Self;
    pub const fn get(self) -> u64;
}

pub struct GitHubObjectId(/* private NonZeroU64 */);
impl GitHubObjectId {
    pub const fn new(value: NonZeroU64) -> Self;
    pub const fn get(self) -> u64;
}

pub struct RepoWatchWorkflowRunAttempt(/* private NonZeroU64 */);
impl RepoWatchWorkflowRunAttempt {
    pub const fn new(value: NonZeroU64) -> Self;
    pub const fn get(self) -> u64;
}

pub struct RepoWatchPattern(/* private String */);
impl RepoWatchPattern {
    pub const MAX_UTF8_BYTES: usize;
    pub fn try_new(value: String) -> Result<Self, RepoWatchTextError>;
    // accessors: as_str(), is_match()
}

pub struct RepoWatchRuleVersion(/* private NonZeroU64 */);
impl RepoWatchRuleVersion {
    pub const V1: Self;
    pub const fn new(value: NonZeroU64) -> Self;
    pub const fn get(self) -> u64;
}

pub enum RepoWatchEventKindNameV1 {
    PullRequestOpened,
    PullRequestClosed,
    PullRequestMerged,
    HeadChanged,
    MergeableStateChanged,
    ChecksCompleted,
    CheckRunCompleted,
    BranchWorkflowRunCompleted,
    ReviewSubmitted,
    ThreadOpened,
    ThreadResolved,
    Labeled,
    Unlabeled,
    BaseAdvanced,
    ReactionChanged,
}

impl RepoWatchEventKindNameV1 {
    pub fn all() -> Vec<Self>;
}

pub enum ChecksOutcome {
    Success,
    Failure,
}

pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
    Stale,
    StartupFailure,
}

pub enum MergeableState {
    Mergeable,
    Conflicting,
    Unknown,
}

pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
}

pub enum ReactionChange {
    Added,
    Removed,
}

pub enum ReactionSubject {
    PullRequestBody,
    IssueComment { id: GitHubObjectId },
    ReviewComment { id: GitHubObjectId },
}

pub enum RepoWatchEventKindV1 {
    PullRequestOpened,
    PullRequestClosed,
    PullRequestMerged,
    HeadChanged { previous: CommitSha, current: CommitSha },
    MergeableStateChanged { current: MergeableState },
    ChecksCompleted { outcome: ChecksOutcome },
    CheckRunCompleted { name: CheckRunName, conclusion: CheckConclusion },
    BranchWorkflowRunCompleted {
        branch: BranchName,
        workflow: WorkflowName,
        conclusion: CheckConclusion,
    },
    ReviewSubmitted {
        reviewer: RepoWatchAuthorLogin,
        state: ReviewState,
        commit: CommitSha,
    },
    ThreadOpened { thread: ReviewThreadId },
    ThreadResolved { thread: ReviewThreadId },
    Labeled { label: LabelName },
    Unlabeled { label: LabelName },
    BaseAdvanced { branch: BranchName },
    ReactionChanged {
        subject: ReactionSubject,
        reactor: RepoWatchAuthorLogin,
        content: ReactionContent,
        change: ReactionChange,
    },
}
impl RepoWatchEventKindV1 {
    pub const fn name(&self) -> RepoWatchEventKindNameV1;
}

pub struct PullRequestEventContext { /* private */ }
pub struct PullRequestEventContextInput {
    pub number: PullRequestNumber,
    pub head_sha: CommitSha,
    pub head_repository: RepositorySlug,
    pub base_branch: BranchName,
    pub head_branch: BranchName,
    pub title: PullRequestTitle,
    pub body: PullRequestBody,
    pub labels: Vec<LabelName>,
    pub draft: bool,
    pub author: Option<RepoWatchAuthorLogin>,
}
impl PullRequestEventContext {
    pub fn new(input: PullRequestEventContextInput) -> Self;
    // accessors: number(), head_sha(), head_repository(), base_branch(),
    //   head_branch(), title(), body(), labels(), draft(), author()
}

pub enum RepoWatchEventTarget {
    PullRequest(PullRequestEventContext),
    Branch,
}

pub struct RepoWatchEvent { /* private */ }
impl RepoWatchEvent {
    pub fn try_pull_request(
        id: RepoWatchEventId,
        repository: RepositorySlug,
        context: PullRequestEventContext,
        kind: RepoWatchEventKindV1,
    ) -> Result<Self, RepoWatchEventConstructionError>;
    pub const fn branch_workflow(
        id: RepoWatchEventId,
        repository: RepositorySlug,
        branch: BranchName,
        workflow: WorkflowName,
        conclusion: CheckConclusion,
    ) -> Self;
    // accessors: id(), repository(), target(), kind()
}

pub enum RepoWatchEventConstructionError {
    BranchKindOnPullRequest,
    HeadChangedCurrentMismatch,
    HeadChangedWithoutChange,
    BaseAdvancedBranchMismatch,
    LabeledContextMissingLabel,
    UnlabeledContextContainsLabel,
}
// implements Error.

pub struct RepoWatchLabelMatcher { /* private */ }
pub struct RepoWatchLabelMatcherInput {
    pub any_of: Vec<LabelName>,
    pub all_of: Vec<LabelName>,
    pub none_of: Vec<LabelName>,
}
impl RepoWatchLabelMatcher {
    pub fn new(input: RepoWatchLabelMatcherInput) -> Self;
    // accessors: any_of(), all_of(), none_of()
}

pub struct RepoWatchMatcherV1 { /* private */ }
pub struct RepoWatchMatcherV1Input {
    pub event_kinds: Vec<RepoWatchEventKindNameV1>,
    pub repository: Option<RepositorySlug>,
    pub base_branch: Option<BranchName>,
    pub head_branch: Option<RepoWatchPattern>,
    pub title: Option<RepoWatchPattern>,
    pub body: Option<RepoWatchPattern>,
    pub labels: RepoWatchLabelMatcher,
    pub draft: Option<bool>,
    pub author: Option<RepoWatchAuthorLogin>,
    pub mergeable_state: Vec<MergeableState>,
    pub conclusion: Vec<CheckConclusion>,
}
impl RepoWatchMatcherV1 {
    pub fn new(input: RepoWatchMatcherV1Input) -> Self;
    pub fn matches(&self, event: &RepoWatchEvent) -> bool;
    // accessors: event_kinds(), repository(), base_branch(), head_branch(),
    //   title(), body(), labels(), draft(), author(), mergeable_state(),
    //   conclusion()
}

pub enum RepoWatchSingletonScope {
    PullRequest,
    Stack,
    Rule,
    Repository,
}

pub enum RepoWatchDispatchContextShape {
    PullRequest,
    Branch,
}

pub struct RepoWatchTemplateContextDeclaration { /* private */ }
impl RepoWatchTemplateContextDeclaration {
    pub fn try_new(
        template: SessionTemplateName,
        accepted: Vec<RepoWatchDispatchContextShape>,
    ) -> Result<Self, RepoWatchTemplateContextDeclarationError>;
    // accessors: template(), accepted(), accepts()
}

pub enum RepoWatchTemplateContextDeclarationError {
    NoAcceptedContextShape { template: SessionTemplateName },
}
// implements Error.

pub struct PullRequestContext { /* private */ }
// sealed: DispatchSessionParameters::try_from_event().
// accessors: repository(), number(), head_sha(), head_repository(), head_branch(),
//            base_branch(), event()

pub struct BranchContext { /* private */ }
// sealed: DispatchSessionParameters::try_from_event().
// accessors: repository(), branch(), workflow(), conclusion(), event()

pub enum DispatchSessionParameters {
    PullRequest(PullRequestContext),
    Branch(BranchContext),
}
impl DispatchSessionParameters {
    pub fn try_from_event(
        event: RepoWatchEvent,
    ) -> Result<Self, RepoWatchDispatchContextError>;
    // accessors: shape(), event()
}

pub enum RepoWatchDispatchContextError {
    InvalidBranchEvent,
}
// implements Error.

pub enum RepoWatchRuleActionV1 {
    DispatchSession { template: SessionTemplateName },
}
impl RepoWatchRuleActionV1 {
    pub const fn template(&self) -> &SessionTemplateName;
}

pub struct DispatchSessionAction { /* private */ }
impl DispatchSessionAction {
    pub const fn new(
        template: SessionTemplateName,
        params: DispatchSessionParameters,
    ) -> Self;
    pub fn synthesized_goal_statement(
        &self,
        rule: &RepoWatchRuleId,
    ) -> Result<GoalStatement, GoalTextError>;
    // accessors: template(), params()
}

pub enum RepoWatchActionV1 {
    DispatchSession(DispatchSessionAction),
}

pub enum RepoWatchRuleValidationError {
    NoActions,
    SubsecondCooldown,
    BranchEventWithPullRequestSingleton { scope: RepoWatchSingletonScope },
    TemplateNotDeclared { template: SessionTemplateName },
    TemplateRejectsContext {
        template: SessionTemplateName,
        shape: RepoWatchDispatchContextShape,
    },
}
// implements Error.

pub struct RepoWatchRuleContentDigest(/* private [u8; 32] */);
impl RepoWatchRuleContentDigest {
    pub const fn as_bytes(&self) -> &[u8; 32];
}

pub enum RepoWatchRuleIdentityField {
    MatcherEventKinds,
    MatcherRepository,
    MatcherBaseBranch,
    MatcherHeadBranchRegex,
    MatcherTitleRegex,
    MatcherBodyRegex,
    MatcherLabelsAnyOf,
    MatcherLabelsAllOf,
    MatcherLabelsNoneOf,
    MatcherDraft,
    MatcherAuthor,
    MatcherMergeableStateAnyOf,
    MatcherConclusionAnyOf,
    Actions,
    SingletonPer,
    CooldownSeconds,
}
impl RepoWatchRuleIdentityField {
    pub const fn configuration_path(self) -> &'static str;
}

pub struct RepoWatchRuleIdentityFieldDigest(/* private [u8; 32] */);
impl RepoWatchRuleIdentityFieldDigest {
    pub const fn as_bytes(&self) -> &[u8; 32];
}

pub struct RepoWatchRule { /* private */ }
impl RepoWatchRule {
    pub fn try_new(
        id: RepoWatchRuleId,
        matcher: RepoWatchMatcherV1,
        actions: Vec<RepoWatchRuleActionV1>,
        singleton_per: RepoWatchSingletonScope,
        cooldown: Duration,
    ) -> Result<Self, RepoWatchRuleValidationError>;
    pub fn required_context_shapes(&self) -> Vec<RepoWatchDispatchContextShape>;
    pub fn validate_template_contexts(
        &self,
        declarations: &[RepoWatchTemplateContextDeclaration],
    ) -> Result<(), RepoWatchRuleValidationError>;
    pub fn content_digest(&self) -> RepoWatchRuleContentDigest;
    pub fn identity_field_digests(
        &self,
    ) -> Vec<(RepoWatchRuleIdentityField, RepoWatchRuleIdentityFieldDigest)>;
    pub fn actions_for_event(
        &self,
        event: &RepoWatchEvent,
    ) -> Result<Vec<RepoWatchActionV1>, RepoWatchDispatchContextError>;
    // accessors: id(), version(), matcher(), actions(), singleton_per(), cooldown()
}
```

## domain: review_workflow

```rust
pub struct ReviewKey(/* private String */);
pub struct ReviewText(/* private String */);
impl ReviewKey {
    pub fn try_new(value: String) -> Result<Self, ReviewValueError>;
    // accessors: as_str(), into_string()
}
impl ReviewText {
    pub fn try_new(value: String) -> Result<Self, ReviewValueError>;
    // accessors: as_str(), into_string()
}

pub enum ReviewValueFailure {
    Empty,
    ContainsNull,
    TooLong { maximum_bytes: usize },
}
pub struct ReviewValueError { /* rejected value and failure */ }
impl ReviewValueError {
    // accessors: failure(), value(), into_parts()
}

pub struct ReviewChangeRequestNumber(/* private NonZeroU64 */);
pub struct ReviewEventOrdinal(/* private NonZeroU32 */);
pub struct ReviewPositiveNumberError;
impl ReviewChangeRequestNumber {
    pub const fn try_new(value: u64) -> Result<Self, ReviewPositiveNumberError>;
    pub const fn get(self) -> u64;
}
impl ReviewEventOrdinal {
    pub const fn one() -> Self;
    pub const fn try_new(value: u32) -> Result<Self, ReviewPositiveNumberError>;
    pub const fn get(self) -> u32;
}

pub struct ReviewConfidence(/* private u16 */);
pub struct ReviewConfidenceError { /* rejected basis points */ }
impl ReviewConfidence {
    pub const fn try_from_basis_points(
        basis_points: u16,
    ) -> Result<Self, ReviewConfidenceError>;
    pub const fn basis_points(self) -> u16;
}
impl ReviewConfidenceError {
    pub const fn basis_points(self) -> u16;
}

pub struct ReviewFindingConfidenceAxes { /* private */ }
impl ReviewFindingConfidenceAxes {
    pub const fn new(
        is_real_confidence: ReviewConfidence,
        severity_label_confidence: ReviewConfidence,
    ) -> Self;
    pub const fn is_real_confidence(self) -> ReviewConfidence;
    pub const fn severity_label_confidence(self) -> ReviewConfidence;
}

pub struct ReviewPolicyVersion(/* private NonZeroU32 */);
pub struct ReviewPolicy { /* version and two confidence thresholds */ }
pub struct ReviewPolicyError { /* rejected complete policy */ }
impl ReviewPolicyVersion {
    pub const fn one() -> Self;
    pub const fn try_new(value: u32) -> Result<Self, ReviewPositiveNumberError>;
    pub const fn get(self) -> u32;
}
impl ReviewPolicy {
    pub const fn try_new(
        version: ReviewPolicyVersion,
        minimum_judge_confidence: ReviewConfidence,
        minimum_publication_confidence: ReviewConfidence,
    ) -> Result<Self, ReviewPolicyError>;
    pub const fn version_one() -> Self;
    // accessors: version(), minimum_judge_confidence(),
    // minimum_publication_confidence()
}
impl ReviewPolicyError {
    pub const fn into_parts(
        self,
    ) -> (ReviewPolicyVersion, ReviewConfidence, ReviewConfidence);
}

pub enum ReviewTargetSubject {
    ChangeRequest(ReviewChangeRequestNumber),
    Commit,
}
pub struct ReviewTargetParentRef { /* target + canonical scope and head */ }
impl ReviewTargetParentRef {
    // accessors: target(), provider(), repository(), head_revision()
}
pub struct ReviewTarget { /* immutable snapshot */ }
pub enum ReviewTargetError {
    MissingChangeRequestBase { target: ReviewTargetId },
    SelfParent { target: ReviewTargetId },
    CyclicParent { target: ReviewTargetId },
    ForeignParent { target: ReviewTargetId },
    MissingParentBase { target: ReviewTargetId },
    DisconnectedParent { target: ReviewTargetId },
    RepeatedChangeRequest { target: ReviewTargetId },
    ParentIdentityMismatch { target: ReviewTargetId },
}
impl ReviewTarget {
    pub fn try_new(
        id: ReviewTargetId,
        provider: ReviewKey,
        repository: ReviewKey,
        subject: ReviewTargetSubject,
        head_revision: ReviewKey,
        base_revision: Option<ReviewKey>,
        stack_parent: Option<&ReviewTarget>,
    ) -> Result<Self, ReviewTargetError>;
    pub fn try_reconstitute(
        id: ReviewTargetId,
        provider: ReviewKey,
        repository: ReviewKey,
        subject: ReviewTargetSubject,
        head_revision: ReviewKey,
        base_revision: Option<ReviewKey>,
        stack_parent: Option<ReviewTargetId>,
        stack_parent_evidence: Option<&ReviewTarget>,
    ) -> Result<Self, ReviewTargetError>;
    // accessors: id(), provider(), repository(), subject(), head_revision(),
    // base_revision(), stack_parent(), ancestry()
}

pub struct ReviewRunRef { /* target + run */ }
pub struct ReviewPassRef { /* run ref + pass */ }
pub struct ReviewFindingRef { /* producing pass ref + finding */ }
impl ReviewRunRef {
    pub const fn new(target: ReviewTargetId, run: ReviewRunId) -> Self;
    // accessors: target(), run()
}
impl ReviewPassRef {
    pub const fn new(run: ReviewRunRef, pass: ReviewPassId) -> Self;
    // accessors: run(), pass(), target()
}
impl ReviewFindingRef {
    pub const fn new(pass: ReviewPassRef, finding: ReviewFindingId) -> Self;
    // accessors: pass(), run(), finding(), target()
}
pub enum ReviewFindingStatus {
    Open,
    Accepted,
    Rejected,
    Duplicate,
    Superseded,
    Stale,
    Posted,
    Fixed,
    BlockedWithReason,
}
pub enum ReviewFindingEventType {
    Accepted,
    Rejected,
    Duplicate,
    Superseded,
    Stale,
    Posted,
    Fixed,
    BlockedWithReason,
}
pub enum ReviewFindingEventResultKind {
    Accepted,
    Rejected { reason: ReviewText },
    Duplicate { canonical: ReviewReferencedFindingEvidence },
    Superseded { successor: ReviewReferencedFindingEvidence },
    Stale,
    Posted { link: ReviewExternalLinkId },
    Fixed,
    BlockedWithReason {
        reason: ReviewText,
        link: Option<ReviewExternalLinkId>,
    },
}
impl ReviewFindingEventResultKind {
    pub const fn event_type(&self) -> ReviewFindingEventType;
}
pub struct ReviewFindingEventResult { /* finding + ordinal + complete payload */ }
impl ReviewFindingEventResult {
    pub fn new(
        finding: ReviewFindingRef,
        ordinal: ReviewEventOrdinal,
        kind: ReviewFindingEventResultKind,
    ) -> Self;
    // accessors: finding(), ordinal(), event_type(), kind()
}
pub struct ReviewProducedFindings { /* canonical bounded identity inventory */ }
impl ReviewProducedFindings {
    pub fn try_new(
        findings: Vec<ReviewFindingRef>,
    ) -> Result<Self, ReviewProducedFindingsError>;
    // accessors: findings(), contains()
}
pub enum ReviewProducedFindingsError {
    TooMany { actual: usize, maximum: usize },
    Duplicate { finding: ReviewFindingRef },
}
pub struct ReviewExternalLinkAttachmentResult {
    /* reservation + object key + optional posted event */
}
impl ReviewExternalLinkAttachmentResult {
    pub const fn new(
        link: ReviewExternalLinkId,
        external_object: ReviewKey,
        finding_event: Option<ReviewFindingEventResult>,
    ) -> Self;
    // accessors: link(), external_object(), finding_event()
}
pub struct ReviewExternalLinkObservationResult {
    /* reservation + ordinal + state */
}
impl ReviewExternalLinkObservationResult {
    pub const fn new(
        link: ReviewExternalLinkId,
        ordinal: ReviewEventOrdinal,
        state: ReviewExternalObjectState,
    ) -> Self;
    // accessors: link(), ordinal(), state()
}
pub struct ReviewExternalLinkNoChangeResult { /* reservation + observation frontier + state */ }
impl ReviewExternalLinkNoChangeResult {
    pub const fn new(
        link: ReviewExternalLinkId,
        observed_through: ReviewEventOrdinal,
        state: ReviewExternalObjectState,
    ) -> Self;
    // accessors: link(), observed_through(), state()
}
pub struct ReviewExternalLinkPublicationBlockedResult {
    /* pending reservation + reason */
}
impl ReviewExternalLinkPublicationBlockedResult {
    pub const fn new(link: ReviewExternalLinkId, reason: ReviewText) -> Self;
    // accessors: link(), reason()
}
pub enum ReviewPassResult {
    ProducedFindings(ReviewProducedFindings),
    FindingEvent(ReviewFindingEventResult),
    ExternalLinkAttachment(ReviewExternalLinkAttachmentResult),
    ExternalLinkObservation(ReviewExternalLinkObservationResult),
    ExternalLinkNoChange(ReviewExternalLinkNoChangeResult),
    ExternalLinkPublicationBlocked(ReviewExternalLinkPublicationBlockedResult),
}
pub struct ReviewReferencedFindingEvidence {
    /* reference + frozen eligible status + authenticated producer policy */
}
impl ReviewReferencedFindingEvidence {
    pub fn try_from_finding(finding: &ReviewFinding) -> Option<Self>;
    pub fn try_reconstitute(
        reference: ReviewFindingRef,
        status: ReviewFindingStatus,
        producing_pass: &ReviewPassEvidence,
        producing_run: ReviewRunEvidence,
    ) -> Option<Self>;
    // accessors: reference(), status(), producer_policy(), producing_pass()
}

pub enum ReviewWorkflowKind {
    ImportExternalContext,
    ReadOnlyReview,
    JudgeFindings,
    DedupeFindings,
    PublishReview,
    FixFindings,
    PropagateStack,
}
pub enum ReviewRunState {
    Queued,
    Running { active_pass: ReviewPassRef },
    Succeeded { concluding_pass: ReviewPassRef },
    Failed { failed_pass: ReviewPassRef },
    Blocked { blocking_pass: ReviewPassRef },
    Cancelled { last_pass: Option<ReviewPassRef> },
}
pub struct ReviewRunEvidence { /* canonical run reference + workflow + policy + state */ }
impl ReviewRunEvidence {
    pub const fn new(
        reference: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        policy: ReviewPolicy,
        state: ReviewRunState,
    ) -> Self;
    // accessors: reference(), workflow(), policy(), state()
}
pub struct ReviewPassEvidence { /* validated pass + canonical run policy */ }
impl ReviewPassEvidence {
    pub fn from_pass(pass: &ReviewPass, policy: ReviewPolicy) -> Self;
    pub fn project_result(&self, result: ReviewPassResult) -> Option<Self>;
    // accessors: reference(), kind(), policy(), state()
}
pub struct ReviewRunReconstitutionInput { /* run row + canonical pass */ }
impl ReviewRunReconstitutionInput {
    pub const fn new(
        reference: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        policy: ReviewPolicy,
        state: ReviewRunState,
        pass_evidence: Option<ReviewPassEvidence>,
    ) -> Self;
    // accessors: reference(), workflow(), policy(), state(), pass_evidence()
}
pub struct ReviewRun { /* reference + kind + policy + state + recorded pass */ }
pub enum ReviewRunEvidenceFailure {
    ForeignPass,
    MissingPassEvidence,
    UnexpectedPassEvidence,
    PassMismatch,
    PassKindMismatch,
    PassPolicyMismatch,
    PassStateMismatch,
}
pub struct ReviewRunReconstitutionError { /* input + failure */ }
impl ReviewRunReconstitutionError {
    // accessors: failure(), input(), into_input()
}
pub enum ReviewRunTransitionFailure {
    Evidence(ReviewRunEvidenceFailure),
    InvalidTransition,
}
pub struct ReviewRunTransitionError { /* unchanged run + requested state + evidence + failure */ }
impl ReviewRun {
    pub const fn new(
        reference: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        policy: ReviewPolicy,
    ) -> Self;
    pub fn try_reconstitute(
        input: ReviewRunReconstitutionInput,
    ) -> Result<Self, ReviewRunReconstitutionError>;
    pub fn transition(
        self,
        next: ReviewRunState,
        pass_evidence: Option<ReviewPassEvidence>,
    ) -> Result<Self, ReviewRunTransitionError>;
    // accessors: reference(), workflow(), policy(), state(), recorded_pass(),
    // evidence()
}
impl ReviewRunTransitionError {
    // accessors: failure(), states(), pass_evidence(), current(), into_current()
}

pub enum ReviewPassKind {
    ImportExternalContext,
    ReadOnlyReview,
    Judge,
    Dedupe,
    Publish,
    Fix,
    PropagateStack,
}
pub enum ReviewPassState {
    Queued,
    Running { turn: TurnId },
    Succeeded {
        turn: TurnId,
        output_frontier: ContextFrontierId,
        result: Option<ReviewPassResult>,
    },
    Failed { turn: TurnId },
    Blocked {
        turn: TurnId,
        result: Option<ReviewPassResult>,
    },
    Cancelled { turn: Option<TurnId> },
}
pub enum ReviewPassTurnOutcome {
    Active,
    Completed,
    Refused,
    Failed,
    Cancelled,
    ReconciliationRequired,
}
pub struct ReviewPassTurnEvidence { /* canonical turn ownership + outcome */ }
impl ReviewPassTurnEvidence {
    pub const fn new(
        turn: TurnId,
        session: SessionId,
        accepted_input: AcceptedInputId,
        outcome: ReviewPassTurnOutcome,
        terminal_frontier: Option<ContextFrontierId>,
    ) -> Self;
    // accessors: turn(), session(), accepted_input(), outcome(),
    // terminal_frontier()
}
pub struct ReviewPassAcceptedInputEvidence {
    /* canonical accepted input + session + optional origin turn */
}
impl ReviewPassAcceptedInputEvidence {
    pub const fn new(
        accepted_input: AcceptedInputId,
        session: SessionId,
        origin_turn: Option<TurnId>,
    ) -> Self;
    // accessors: accepted_input(), session(), origin_turn()
}
pub struct ReviewPassReconstitutionInput { /* pass row + canonical evidence */ }
impl ReviewPassReconstitutionInput {
    pub const fn new(
        reference: ReviewPassRef,
        kind: ReviewPassKind,
        workflow_run: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        session: SessionId,
        accepted_input: AcceptedInputId,
        accepted_input_evidence: ReviewPassAcceptedInputEvidence,
        state: ReviewPassState,
        turn_evidence: Option<ReviewPassTurnEvidence>,
    ) -> Self;
    // accessors: reference(), kind(), workflow_run(), workflow(), session(),
    // accepted_input(), accepted_input_evidence(), state(), turn_evidence()
}
pub struct ReviewPass { /* reference + session input + origin turn + state */ }
pub enum ReviewPassConstructionFailure {
    ForeignRun,
    RunWorkflowMismatch,
    RunNotQueued,
    RunAlreadyHasPass,
    AcceptedInputSessionMismatch,
    AcceptedInputHasNoOriginTurn,
}
pub struct ReviewPassConstructionError { /* rejected construction facts */ }
impl ReviewPassConstructionError {
    // accessors: reference(), kind(), workflow(), run_evidence(), sessions(),
    // accepted_input(), origin_turn(), failure()
}
pub enum ReviewPassReconstitutionFailure {
    ForeignWorkflowRun,
    RunWorkflowMismatch,
    AcceptedInputEvidenceMismatch,
    AcceptedInputSessionMismatch,
    AcceptedInputHasNoOriginTurn,
    MissingTurnEvidence,
    UnexpectedTurnEvidence,
    TurnMismatch,
    TurnOriginMismatch,
    TurnSessionMismatch,
    TurnAcceptedInputMismatch,
    TurnOutcomeMismatch,
    TurnFrontierShapeMismatch,
    OutputFrontierMismatch,
    IncompatibleResult,
    ForeignResultTarget,
}
pub struct ReviewPassReconstitutionError { /* input + failure */ }
impl ReviewPassReconstitutionError {
    // accessors: failure(), input(), into_input()
}
pub enum ReviewPassTransitionFailure {
    Evidence(ReviewPassReconstitutionFailure),
    InvalidTransition,
    TurnChanged,
    TurnNotActive,
    IncompatibleResult,
    ResultAlreadyBound,
}
pub struct ReviewPassTransitionError { /* unchanged pass + requested state + evidence + failure */ }
impl ReviewPass {
    pub fn try_new(
        reference: ReviewPassRef,
        kind: ReviewPassKind,
        run: &mut ReviewRun,
        session: SessionId,
        accepted_input: ReviewPassAcceptedInputEvidence,
    ) -> Result<Self, ReviewPassConstructionError>;
    pub fn try_reconstitute(
        input: ReviewPassReconstitutionInput,
    ) -> Result<Self, ReviewPassReconstitutionError>;
    pub fn transition(
        self,
        next: ReviewPassState,
        turn_evidence: Option<ReviewPassTurnEvidence>,
    ) -> Result<Self, ReviewPassTransitionError>;
    pub fn bind_result(
        self,
        result: ReviewPassResult,
    ) -> Result<Self, ReviewPassTransitionError>;
    // accessors: reference(), kind(), session(), accepted_input(), origin_turn(),
    // state()
}
impl ReviewPassTransitionError {
    // accessors: failure(), states(), turn_evidence(), current(), into_current()
}

pub enum ReviewFindingDiffSide {
    Left,
    Right,
}
pub struct ReviewLineRange { /* positive closed range */ }
pub enum ReviewLineRangeError {
    ZeroEndpoint,
    EndBeforeStart,
}
impl ReviewLineRange {
    pub const fn try_new(start: u32, end: u32) -> Result<Self, ReviewLineRangeError>;
    // accessors: start(), end()
}
pub struct ReviewFindingLocation { /* path + optional range + side */ }
impl ReviewFindingLocation {
    pub const fn new(
        file_path: ReviewKey,
        line_range: Option<ReviewLineRange>,
        diff_side: Option<ReviewFindingDiffSide>,
    ) -> Self;
    // accessors: file_path(), line_range(), diff_side()
}

pub enum ReviewFindingSeverity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}
pub struct ReviewFindingContent { /* immutable checked content */ }
impl ReviewFindingContent {
    pub const fn new(
        location: ReviewFindingLocation,
        title: ReviewText,
        body: ReviewText,
        severity: ReviewFindingSeverity,
        confidence_axes: ReviewFindingConfidenceAxes,
        category: ReviewKey,
        recommended_fix: Option<ReviewText>,
    ) -> Self;
    // accessors: location(), title(), body(), severity(),
    // is_real_confidence(), severity_label_confidence(), category(),
    // recommended_fix()
}

pub struct ReviewFindingProposal { /* reference + producing pass + content */ }
impl ReviewFindingProposal {
    pub fn try_new(
        reference: ReviewFindingRef,
        producing_pass: ReviewPassEvidence,
        producing_run: ReviewRunEvidence,
        target: &ReviewTarget,
        content: ReviewFindingContent,
    ) -> Result<Self, ReviewFindingTransitionError>;
    // accessors: reference(), producing_pass(), content()
}
pub struct ReviewFindingExternalLinkRef { /* finding + link + attachment pass */ }
pub enum ReviewFindingExternalLinkFailure {
    ForeignAssociation,
    IncompatibleObjectKind,
    NotAttached,
    AlreadyAttached,
}
pub struct ReviewFindingExternalLinkError { /* canonical association + failure */ }
impl ReviewFindingExternalLinkError {
    // accessors: finding(), link(), association(), failure()
}
impl ReviewFindingExternalLinkRef {
    pub fn try_new(
        finding: ReviewFindingRef,
        link: &ReviewExternalLink,
    ) -> Result<Self, ReviewFindingExternalLinkError>;
    // accessors: finding(), link(), attachment_pass()
}
pub struct ReviewFindingPendingExternalLinkRef { /* finding + pending link */ }
impl ReviewFindingPendingExternalLinkRef {
    pub fn try_new(
        finding: ReviewFindingRef,
        link: &ReviewExternalLink,
    ) -> Result<Self, ReviewFindingExternalLinkError>;
    // accessors: finding(), link()
}

pub enum ReviewFindingEventKind {
    Accepted,
    Rejected { reason: ReviewText },
    Duplicate { canonical: ReviewReferencedFindingEvidence },
    Superseded { successor: ReviewReferencedFindingEvidence },
    Stale,
    Posted { link: Box<ReviewFindingExternalLinkRef> },
    Fixed,
    BlockedWithReason {
        reason: ReviewText,
        link: Option<Box<ReviewFindingPendingExternalLinkRef>>,
    },
}
impl ReviewFindingEventKind {
    pub const fn event_type(&self) -> ReviewFindingEventType;
}
pub struct ReviewFindingEvent { /* finding + ordinal + pass + run + kind */ }
impl ReviewFindingEvent {
    pub const fn new(
        finding: ReviewFindingRef,
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassRef,
        pass_evidence: ReviewPassEvidence,
        run: ReviewRunEvidence,
        kind: ReviewFindingEventKind,
    ) -> Self;
    // accessors: finding(), ordinal(), pass(), pass_evidence(), run_evidence(),
    // kind()
}
pub struct ReviewFinding { /* proposal + history + derived status + replay indexes */ }
impl ReviewFinding {
    pub const fn new(proposal: ReviewFindingProposal) -> Self;
    pub fn try_reconstitute(
        proposal: ReviewFindingProposal,
        events: Vec<ReviewFindingEvent>,
    ) -> Result<Self, ReviewFindingTransitionError>;
    pub fn apply(self, event: ReviewFindingEvent)
        -> Result<Self, ReviewFindingTransitionError>;
    // accessors: proposal(), events(), status()
}
pub fn validate_complete_review_finding_reference_graph(
    findings: &[ReviewFinding],
) -> Result<(), Box<ReviewFindingReferenceGraphError>>;
// impl Display + std::error::Error
pub enum ReviewFindingReferenceGraphError {
    DuplicateFinding { reference: ReviewFindingRef },
    ForeignTargetRoot {
        expected: ReviewTargetId,
        actual: ReviewTargetId,
    },
    MissingReferencedFinding {
        finding: ReviewFindingRef,
        referenced: ReviewFindingRef,
    },
    ReferencedFindingPolicyMismatch {
        finding: ReviewFindingRef,
        referenced: ReviewFindingRef,
    },
    Cycle {
        finding: ReviewFindingRef,
        referenced: ReviewFindingRef,
    },
}
pub enum ReviewFindingTransitionFailure {
    ForeignTarget,
    MissingDiffBase,
    ForeignProducingPass,
    ForeignProducingRun,
    IncompatibleProducingRunEvidence,
    IncompatibleProducingPassEvidence,
    ForeignEventFinding,
    ForeignEventPass,
    EventPassEvidenceMismatch,
    IncompatibleEventRunEvidence,
    EventPolicyMismatch,
    ConflictingPassEvidence,
    ConflictingRunEvidence,
    IncompatibleEventPassEvidence,
    BelowJudgmentThreshold,
    BelowPublicationThreshold,
    ForeignReferencedFinding,
    ReferencedFindingPolicyMismatch,
    IneligibleReferencedFinding,
    SelfReference,
    ForeignExternalLink,
    PublicationPassMismatch,
    ReusedPublicationLink,
    NoncontiguousOrdinal { expected: Option<ReviewEventOrdinal> },
    InvalidTransition { current: ReviewFindingStatus },
}
pub struct ReviewFindingTransitionError { /* optional unchanged finding + rejected event + failure */ }
impl ReviewFindingTransitionError {
    // accessors: failure(), current(), event(), into_parts()
}

pub enum ReviewExternalLinkAssociation {
    Target(ReviewTargetId),
    Run(ReviewRunRef),
    Finding(ReviewFindingRef),
}
impl ReviewExternalLinkAssociation {
    pub const fn target(self) -> ReviewTargetId;
}
pub enum ReviewExternalObjectKind {
    ChangeRequest,
    Commit,
    Review,
    ReviewThread,
    ReviewComment,
    ChangeRequestComment,
}
pub struct ReviewExternalLinkAttachment { /* link + stored pass + pass/run evidence + object key */ }
impl ReviewExternalLinkAttachment {
    pub const fn new(
        link: ReviewExternalLinkId,
        pass: ReviewPassRef,
        pass_evidence: ReviewPassEvidence,
        run: ReviewRunEvidence,
        external_object: ReviewKey,
    ) -> Self;
    // accessors: link(), pass(), pass_evidence(), run_evidence(),
    // external_object()
}
pub enum ReviewExternalObjectState {
    Current,
    Outdated,
    Resolved,
}
pub struct ReviewExternalLinkObservation { /* link + ordinal + stored pass + pass/run evidence + state */ }
impl ReviewExternalLinkObservation {
    pub const fn new(
        link: ReviewExternalLinkId,
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassRef,
        pass_evidence: ReviewPassEvidence,
        run: ReviewRunEvidence,
        state: ReviewExternalObjectState,
    ) -> Self;
    // accessors: link(), ordinal(), pass(), pass_evidence(), run_evidence(),
    // state()
}
pub struct ReviewExternalLinkClaim { /* pass + run */ }
impl ReviewExternalLinkClaim {
    pub const fn new(pass: ReviewPassEvidence, run: ReviewRunEvidence) -> Self;
    // accessors: pass(), pass_evidence(), run_evidence()
}
pub struct ReviewExternalLink { /* reservation + attachment + observations + consumed claims */ }
impl ReviewExternalLink {
    pub fn try_reserve(
        id: ReviewExternalLinkId,
        association: ReviewExternalLinkAssociation,
        provider: ReviewKey,
        object_kind: ReviewExternalObjectKind,
        target: &ReviewTarget,
    ) -> Result<Self, ReviewExternalLinkTransitionFailure>;
    pub fn try_reconstitute(
        id: ReviewExternalLinkId,
        association: ReviewExternalLinkAssociation,
        provider: ReviewKey,
        object_kind: ReviewExternalObjectKind,
        attachment: Option<ReviewExternalLinkAttachment>,
        observations: Vec<ReviewExternalLinkObservation>,
        claims: Vec<ReviewExternalLinkClaim>,
        target: &ReviewTarget,
    ) -> Result<Self, ReviewExternalLinkTransitionFailure>;
    pub fn attach(self, attachment: ReviewExternalLinkAttachment)
        -> Result<Self, ReviewExternalLinkTransitionError>;
    pub fn observe(self, observation: ReviewExternalLinkObservation)
        -> Result<Self, ReviewExternalLinkTransitionError>;
    pub fn confirm_unchanged(
        self,
        pass: ReviewPassEvidence,
        run: ReviewRunEvidence,
    ) -> Result<Self, ReviewExternalLinkTransitionError>;
    pub fn block_publication(
        self,
        pass: ReviewPassEvidence,
        run: ReviewRunEvidence,
    ) -> Result<Self, ReviewExternalLinkTransitionError>;
    // accessors: id(), association(), provider(), object_kind(), attachment(),
    // observations(), claims()
}
pub struct ReviewExternalObjectClaim {
    /* target + provider + repository + subject + canonical object */
}
impl ReviewExternalObjectClaim {
    pub fn try_new(
        link: &ReviewExternalLink,
        target: &ReviewTarget,
    ) -> Result<Self, ReviewExternalObjectClaimError>;
    pub fn validate_reassociation(
        &self,
        candidate: &Self,
    ) -> Result<(), ReviewExternalObjectClaimError>;
    // accessors: target(), external_object()
}
pub enum ReviewExternalObjectClaimError {
    ForeignTarget,
    ProviderMismatch,
    NotAttached,
    DifferentObject,
    SameTarget,
    UnrelatedTarget,
}
pub struct ReviewExternalLinkTransitionError { /* unchanged link + failure */ }
impl ReviewExternalLinkTransitionError {
    // accessors: current(), failure(), into_parts()
}
pub enum ReviewExternalLinkTransitionFailure {
    ForeignAssociationTarget,
    ProviderMismatch,
    AlreadyAttached,
    ForeignAttachmentLink,
    ForeignObservationLink,
    ForeignPass,
    IncompatibleAttachmentPass,
    AttachmentPassEvidenceMismatch,
    IncompatibleAttachmentRunEvidence,
    IncompatibleObservationPass,
    ObservationPassEvidenceMismatch,
    IncompatibleObservationRunEvidence,
    IncompatiblePublicationBlockPass,
    IncompatiblePublicationBlockRunEvidence,
    UnchangedObservation,
    ConflictingPassEvidence,
    ConflictingRunEvidence,
    NotAttached,
    NoncontiguousOrdinal { expected: Option<ReviewEventOrdinal> },
}
```

## Inventory

| Module                                             | Public types                     |
| -------------------------------------------------- | -------------------------------- |
| domain: lib.rs identities                          | 30                               |
| domain: actor                                      | 1                                |
| domain: blob                                       | 10                               |
| domain: program_journal                            | 25                               |
| domain: imported_conversation                      | 32 (+5 free fn)                  |
| domain: session_template                           | 6                                |
| domain: session_placement                          | 18                               |
| domain: git_remote                                 | 4 (+2 free fn)                   |
| domain: session                                    | 22                               |
| domain: session_delegation                         | 37 (+3 free fn)                  |
| domain: session_lifecycle                          | 23                               |
| domain: session_lifecycle_command                  | 9                                |
| domain: imported_session                           | 20                               |
| domain: configuration                              | 24                               |
| domain: model_settings                             | 25                               |
| domain: accepted_input                             | 5                                |
| domain: delivery_request                           | 2                                |
| domain: user_content                               | 15                               |
| domain: submit_input                               | 37                               |
| domain: queue_order                                | 5 (+1 free fn)                   |
| domain: repo_watch                                 | 51                               |
| domain: turn_lifecycle                             | 11                               |
| domain: turn_eligibility                           | 39                               |
| domain: turn_attempt                               | 13                               |
| domain: model_call                                 | 12                               |
| domain: context_compaction                         | 12                               |
| domain: model_execution                            | 54                               |
| domain: context_frontier                           | 6                                |
| domain: semantic_entry                             | 6                                |
| domain: tool                                       | 53                               |
| domain: tool_attempt                               | 27                               |
| domain: tool_execution                             | 20                               |
| domain: provider_evidence                          | 5                                |
| domain: applied_interrupt                          | 2                                |
| domain: fatal_mismatch                             | 0                                |
| domain: replace_session_defaults                   | 13                               |
| domain: goal                                       | 26                               |
| domain: goal_command                               | 5                                |
| domain: review_workflow                            | 83 (+1 free fn)                  |
| domain: session_metadata                           | 15                               |
| domain: runner                                     | 70                               |
| domain: workspace                                  | 4                                |
| domain: workspace_instruction                      | 18                               |
| **signalbox-domain total**                         | **895 (+12 free fn)**            |
| application: repo_watch_operations                 | 33 (+2 free fn) (incl. 1 trait)  |
| application: approval_judge                        | 8 (incl. 1 trait)                |
| application: attention                             | 17 (+6 free fn) (incl. 1 trait)  |
| application: blob_derivation                       | 9 (incl. 3 traits)               |
| application: commissioned_dispatch                 | 6 (incl. 1 trait)                |
| application: conversation_import                   | 12 (incl. 4 traits)              |
| application: create_session                        | 8 (incl. 2 traits)               |
| application: update_session_placement              | 4 (incl. 1 trait)                |
| application: create_session_from_imported_frontier | 6 (incl. 2 traits)               |
| application: list_conversations                    | 8 (incl. 2 traits)               |
| application: load_session                          | 2 (incl. 1 trait)                |
| application: search                                | 22 (+5 free fn) (incl. 2 traits) |
| application: usage                                 | 37 (+4 free fn) (incl. 1 trait)  |
| application: session_timeline                      | 29 (+7 free fn) (incl. 1 trait)  |
| application: session_live                          | 9 (+1 free fn) (incl. 1 trait)   |
| application: model_execution                       | 41 (incl. 8 traits)              |
| application: tool_loop                             | 28 (incl. 5 traits)              |
| application: operator_failure                      | 2 (incl. 1 trait)                |
| application: session_delegation                    | 1 (incl. 1 trait)                |
| application: replace_session_defaults              | 5 (incl. 1 trait)                |
| application: convergence_reconciliation            | 6 (+1 free fn)                   |
| application: repo_watch                            | 49 (+3 free fn) (incl. 4 traits) |
| application: repo_watch_webhook                    | 18 (+2 free fn)                  |
| application: review_orchestration                  | 37 (incl. 2 traits)              |
| application: review_workflow                       | 9 (incl. 2 traits)               |
| application: session_metadata                      | 12 (incl. 4 traits)              |
| application: scheduler                             | 20 (+1 free fn) (incl. 7 traits) |
| application: start_eligible_turn                   | 5 (incl. 2 traits)               |
| application: startup_scan                          | 7 (incl. 2 traits)               |
| application: submit_input                          | 7 (incl. 2 traits)               |
| application: tool_dispatch_gate                    | 2                                |
| application: tool_execution_test_support           | 7 (+1 free fn)                   |
| application: tool_loop_ports                       | 10 (incl. 3 traits)              |
| application: turn_liveness                         | 16                               |
| application: workspace_instructions                | 5 (+1 free fn)                   |
| **signalbox-application total**                    | **497 (+34 free fn)**            |
