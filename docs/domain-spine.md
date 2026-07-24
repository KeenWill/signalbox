# Domain spine

This file is the owner's primary API-review surface. The crates are
authoritative: this is a hand-maintained mirror of their public API, updated
from source and never edited in source's place. The source files in
`crates/domain/src/` and `crates/application/src/` are intentionally dense with
rustdoc, unit tests, and `compile_fail` proofs; domain shape is reviewed here
instead. The mirror covers the public type and function surface of
`signalbox-domain` and `signalbox-application` as bare declarations — no doc
comments, no tests, no bodies. Any pull request that adds, removes, or changes a
public item in either crate must update this file in the same change;
`AGENTS.md` carries that rule, and CI (`scripts/check_domain_spine.py`) fails
when an exported name is missing here or an inventory count disagrees with
source.

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

The eleven identities defined in `lib.rs`:

```rust
pub struct DurableCommandId(/* private */);
pub struct SessionId(/* private */);
pub struct ImportedConversationId(/* private */);
pub struct ImportedTranscriptEntryId(/* private */);
pub struct AcceptedInputId(/* private */);
pub struct TurnId(/* private */);
pub struct TurnAttemptId(/* private */);
pub struct ModelCallId(/* private */);
pub struct ProviderTargetEvidenceId(/* private */);
pub struct ToolRequestId(/* private */);
pub struct ToolAttemptId(/* private */);
```

Five more identities with the same shape are defined in their owning modules and
listed there: `DirectModelSelection`, `ModelAlias` (configuration),
`ProviderModelIdentity` (model_call), `ContextFrontierId`,
`SemanticTranscriptEntryId` (context_frontier).

## domain: actor

```rust
pub enum Actor {
    Owner,
    Model { turn: TurnId },
    Recovery,
    Tool { request: ToolRequestId },
}
```

## domain: imported_conversation

```rust
pub enum ImportedConversationFormat {
    ClaudeCodeSessionJsonlV1,
}

pub struct ImportedRawRecordHash(/* private [u8; 32] */);
impl ImportedRawRecordHash {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self;
    pub const fn as_bytes(&self) -> &[u8; 32];
    pub fn digest(bytes: &[u8]) -> Self;
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
    // accessors: content_hash(), bytes(), normalized()
}
// Debug redacts bytes and normalized content.

pub struct ImportedRawSourceRecordReconstitutionInput { /* private */ }
impl ImportedRawSourceRecordReconstitutionInput {
    pub fn new(
        position: ImportedRawRecordPosition,
        stored_hash: ImportedRawRecordHash,
        bytes: Vec<u8>,
        normalized: ImportedStructuredValue,
    ) -> Self;
    // accessors: position(), stored_hash(), bytes(), normalized()
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
// sealed: ImportedConversationReconstitutionInput::reconstitute
impl ImportedTranscriptEntry {
    // accessors: identity(), conversation(), position(), raw_record_position(),
    //   record_entry_position(), source_speaker(), content(), source()
}

pub struct ImportedTranscriptFrontier { /* private */ }
// sealed: ImportedConversation frontier methods
impl ImportedTranscriptFrontier {
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
    RawRecordNormalizedValueNotObject {
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
    PositionExhausted,
}

pub struct ImportedConversationReconstitutionError { /* private */ }
// sealed: Err of ImportedConversationReconstitutionInput::reconstitute
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
```

## domain: session

```rust
pub enum SessionCreationCause {
    OwnerInitiated,
}

pub struct TranscriptFrontier { /* private */ }
// sealed: no public producer in this slice; the later semantic-history slice
// supplies the trusted frontier producer. Copy; equality is exact-boundary.

pub enum TranscriptAncestry {
    None,
    SingleSource {
        source_session: SessionId,
        source_frontier: TranscriptFrontier,
    },
}

pub struct SessionCreationProvenance { /* private */ }
impl SessionCreationProvenance {
    pub const fn new(cause: SessionCreationCause, ancestry: TranscriptAncestry) -> Self;
    // accessors: cause(), ancestry()
}

pub struct CreateSession { /* private */ }
impl CreateSession {
    pub const fn new(
        command_id: DurableCommandId,
        provenance: SessionCreationProvenance,
        initial_configuration_defaults: SessionConfigurationDefaults,
    ) -> Self;
    pub const fn establish_initial_defaults(&self) -> VersionedSessionConfigurationDefaults;
    pub fn prepare(self, session: SessionId)
        -> Result<PreparedCreateSession, CreateSessionPreparationError>;
    // accessors: command_id(), provenance(), initial_configuration_defaults()
}
// Eq/Hash exclude command_id (comparison-payload rule,
// spec/identity-and-commands.md)

pub struct InitialSession { /* private */ }
// sealed: carried only by PreparedCreateSession::session and
// ReconstitutedSessionCreation::session
impl InitialSession {
    // accessors: id(), provenance(), configuration_defaults()
}

pub struct Session { /* private */ }
// sealed: SessionReconstitutionInput::reconstitute
// non-Copy: owned snapshot, cloned deliberately (session aggregate,
// spec/sessions-and-transcript.md)
impl Session {
    // accessors: id(), creation_provenance(), current_configuration_defaults()
}

pub struct SessionReconstitutionInput { /* private */ }
impl SessionReconstitutionInput {
    pub const fn new(
        requested_session: SessionId,
        stored_session: SessionId,
        provenance: SessionCreationProvenance,
        current_defaults_session: SessionId,
        current_defaults_version: SessionConfigurationDefaultsVersion,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    ) -> Self;
    pub fn reconstitute(self) -> Result<Session, SessionReconstitutionError>;
    // accessors: requested_session(), stored_session(), provenance(),
    //   current_defaults_session(), current_defaults_version(),
    //   defaults_session(), defaults_version(), defaults()
}

pub enum SessionReconstitutionFailure {
    RequestedSessionMismatch,
    CurrentDefaultsSessionMismatch,
    DefaultsSessionMismatch,
    CurrentDefaultsVersionMismatch,
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
    pub fn reconstitute(self)
        -> Result<ReconstitutedSessionCreation, CreateSessionReconstitutionError>;
    // accessors: command(), result_session(), session(), provenance(),
    //   defaults_session(), defaults_version(), defaults()
}

pub enum CreateSessionReconstitutionFailure {
    SessionResultMismatch,
    ProvenanceMismatch,
    DefaultsSessionMismatch,
    TranscriptAncestryUnavailable,
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
    // accessors: model(), parameters(), known_provider_failure_retry(), model_fallback()
}

pub struct SessionConfigurationDefaultsVersion(/* private u64 */);
impl SessionConfigurationDefaultsVersion {
    pub const fn try_from_u64(value: u64) -> Option<Self>;  // None for zero
    pub const fn as_u64(self) -> u64;
    pub const fn first() -> Self;
    pub const fn checked_next(self) -> Option<Self>;  // None at u64::MAX
}

pub struct SessionConfigurationDefaults { /* private */ }
impl SessionConfigurationDefaults {
    pub const fn new(model: ModelSelectionRequest) -> Self;
    // accessors: model()
}

pub struct VersionedSessionConfigurationDefaults { /* private */ }
impl VersionedSessionConfigurationDefaults {
    pub const fn establish(defaults: SessionConfigurationDefaults) -> Self;  // version one
    pub fn replace(self, defaults: SessionConfigurationDefaults) -> Option<Self>;
    pub fn derive_request(
        &self,
        expected: SessionConfigurationDefaultsVersion,
        model: ModelSelectionOverride,
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
    // accessors: model()
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
    ) -> Result<Self, UnknownModelAlias>;
    // accessors: requested(), session_defaults_version(), effective()
}

pub struct UnknownModelAlias { /* private */ }
// sealed: Err of OriginConfiguration::freeze
impl UnknownModelAlias {
    // accessors: alias()
}

pub enum TurnConfigurationProvenance {
    ExplicitOrigin(OriginConfiguration),
    InheritedForReclassifiedSteering(SteeringBinding),
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
    // accessors: id(), disposition()
}

pub enum AcceptedInputLifecycleTransitionError {
    CannotConsumeAsSteering { lifecycle: AcceptedInputLifecycle },
    CannotReclassifyAsTurnOrigin { lifecycle: AcceptedInputLifecycle },
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
    // accessors: expected_session_defaults_version(), model()
}

pub enum DeliveryRequest {
    StartWhenNoActiveTurn {
        configuration: PerInputConfigurationChoices,
    },
    Interrupt {
        expected_active_turn: TurnId,
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

pub enum NonEmptyUnicodeTextFailure {
    Empty,
    ContainsNull,
}

pub struct NonEmptyUnicodeTextError { /* private */ }
impl NonEmptyUnicodeTextError {
    pub fn into_parts(self) -> (String, NonEmptyUnicodeTextFailure);
    // accessors: failure(), value()
}

pub enum UserContent {
    Text { value: NonEmptyUnicodeText },
}
impl UserContent {
    pub fn try_text(value: String) -> Result<Self, NonEmptyUnicodeTextError>;
    // accessors: text()
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
    pub fn prepare_session_not_found(self) -> PreparedSubmitInput;
    pub fn prepare_when_no_active_turn(
        self,
        session: &Session,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        previous_position: Option<SessionInputPosition>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError>;
    pub fn prepare_with_active_turn(
        self,
        scheduling: &AcceptedInputSchedulingProjection,
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
}

pub struct SubmitInputTerminalSourceReconstitutionInput { /* private */ }
impl SubmitInputTerminalSourceReconstitutionInput {
    pub fn new(
        origin: SubmitInputTurnOriginReconstitutionInput,
        turn: TurnId,
        disposition: TurnDisposition,
    ) -> Self;
    pub fn interrupted_model_call_reconciliation(
        origin: SubmitInputTurnOriginReconstitutionInput,
        turn: TurnId,
        ambiguous_call: ModelCallId,
        interrupt: AppliedInterruptProof,
    ) -> Self;
}

pub struct SubmitInputTurnOriginReconstitutionInput { /* private */ }
impl SubmitInputTurnOriginReconstitutionInput {
    pub fn new(
        receipt: ReconstitutedSubmitInput,
        lifecycle: AcceptedInputLifecycle,
        queue_accepted_input: AcceptedInputId,
        queue_session: SessionId,
        queue_turn: TurnId,
        queue_order: AcceptedInputQueueOrder,
    ) -> Self;
    pub fn reclassified(
        receipt: ReconstitutedSubmitInput,
        lifecycle: AcceptedInputLifecycle,
        queue_accepted_input: AcceptedInputId,
        queue_session: SessionId,
        queue_turn: TurnId,
        queue_order: AcceptedInputQueueOrder,
        source_terminal: SubmitInputTerminalSourceReconstitutionInput,
    ) -> Self;
}

pub struct SubmitInputReconstitutionInput { /* private */ }
impl SubmitInputReconstitutionInput {
    pub fn applied_turn_origin(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_accepted_input: AcceptedInputId,
        result_turn: TurnId,
        predecessor_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
        accepted_command: DurableCommandId,
        accepted_input: AcceptedInputId,
        accepted_session: SessionId,
        accepted_content: UserContent,
        accepted_delivery: DeliveryRequest,
        accepted_position: SessionInputPosition,
        accepted_disposition: AcceptedInputDisposition,
        queue_session: SessionId,
        queue_turn: TurnId,
        queue_order: AcceptedInputQueueOrder,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        stored_requested_model: ModelSelectionRequest,
        stored_frozen_model: FrozenModelSelection,
    ) -> Self;
    pub fn applied_pending_steering(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_accepted_input: AcceptedInputId,
        result_source_turn: TurnId,
        source_turn_origin: SubmitInputTurnOriginReconstitutionInput,
        accepted_command: DurableCommandId,
        accepted_input: AcceptedInputId,
        accepted_session: SessionId,
        accepted_content: UserContent,
        accepted_delivery: DeliveryRequest,
        accepted_position: SessionInputPosition,
    ) -> Self;
    pub const fn rejected_safe_point_unavailable_while_stopping(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_active_turn: TurnId,
        active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
        existing_interrupt: AppliedInterruptCommandResult,
    ) -> Self;
    pub const fn rejected_interrupt_already_applied(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_active_turn: TurnId,
        result_existing_command: DurableCommandId,
        active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
        existing_interrupt: AppliedInterruptCommandResult,
    ) -> Self;
    pub const fn rejected_session_not_found(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
    ) -> Self;
    pub const fn rejected_no_active_turn(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_expected_active_turn: TurnId,
    ) -> Self;
    pub const fn rejected_active_turn_present(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_active_turn: TurnId,
        active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
    ) -> Self;
    pub const fn rejected_active_turn_mismatch(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_expected_active_turn: TurnId,
        result_actual_active_turn: TurnId,
        actual_turn_origin: SubmitInputTurnOriginReconstitutionInput,
    ) -> Self;
    pub const fn rejected_defaults_version_mismatch(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_expected: SessionConfigurationDefaultsVersion,
        result_current: SessionConfigurationDefaultsVersion,
        active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    ) -> Self;
    pub const fn rejected_unknown_model_alias(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_alias: ModelAlias,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    ) -> Self;
    pub const fn rejected_acceptance_position_exhausted(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_last_position: SessionInputPosition,
        active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    ) -> Self;
    // no SafePointUnavailableWhileStopping replay constructor until its exact
    // owner-correlated StopRequested evidence projection exists
    pub fn reconstitute(self)
        -> Result<ReconstitutedSubmitInput, SubmitInputReconstitutionError>;
    // accessors: command()
}

pub enum SubmitInputReconstitutionFailure {
    StoredActorMismatch,
    AppliedDeliveryIsNotTurnOrigin,
    AppliedDeliveryIsNotNextSafePoint,
    ResultSessionMismatch,
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
    OwnerChoseReconciliation { decision: AppliedStopForReconciliationProof },
    InterruptRequiresReconciliation { interrupt: AppliedInterruptProof },
    FatalMismatchRequiresReconciliation { causes: FatalMismatchStopCauses },
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
    AwaitingRecoveryDecision {
        ambiguous_operations: NonEmptyIssuedOperationRefs,
        applied_interrupt: Option<AppliedInterruptProof>,
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
        interrupt: AppliedInterruptCommandResult,
        terminal_frontier: ContextFrontierId,
    },
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
    // accessors: owning_turn(), ended_attempt(), attempt_end(), ended_call()
}

pub struct TerminalAttemptEndReconstitutionInput { /* private */ }
impl TerminalAttemptEndReconstitutionInput {
    pub const fn without_stop(disposition: UnstoppedAttemptDisposition) -> Self;
    pub const fn after_cancellation(
        disposition: CancellationStopDisposition,
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
    pub const fn stop_requested(
        owning_turn: TurnId,
        current_attempt: TurnAttemptId,
        call: ModelCallId,
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

pub struct PendingSteeringInput { /* private */ }
// sealed: checked AcceptedInputSchedulingProjection::active_turn_execution
impl PendingSteeringInput {
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
    // accessors: stored_session(), turn(), accepted_input_session(),
    // accepted_input(), queue_session(), queue_turn(), order(),
    // origin_delivery(), origin_configuration(), configuration_provenance(), state()
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
    pub fn with_consumed_steering_facts(
        self,
        consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    ) -> Self;
    // accessors: session(), turns(), semantic_entries(), snapshots(),
    // pinned_targets(), model_calls(), consumed_steering(),
    // active_acceptance_tail()
}

pub enum AcceptedInputSchedulingReconstitutionFailure {
    UnsupportedSessionAncestry,
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
    ConsumedSteeringSessionMismatch { accepted_input: AcceptedInputId },
    DuplicateConsumedSteering { accepted_input: AcceptedInputId },
    SteeringSemanticEntryMismatch { entry: SemanticTranscriptEntryId },
    ConsumedSteeringMismatch { accepted_input: AcceptedInputId },
    UnsupportedSemanticEntry { entry: SemanticTranscriptEntryId },
    SemanticEntryCallMissing {
        entry: SemanticTranscriptEntryId,
        call: ModelCallId,
    },
    SemanticEntryCallMismatch {
        entry: SemanticTranscriptEntryId,
        call: ModelCallId,
    },
    DuplicateModelCall { call: ModelCallId },
    DuplicatePinnedTarget { turn: TurnId },
    PinnedTargetMissing { call: ModelCallId },
    UnreferencedPinnedTarget { turn: TurnId },
    ModelCallSnapshotMissing { call: ModelCallId },
    InvalidModelCall { call: ModelCallId },
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
    pub fn apply_interrupt_to_model_call_recovery(
        self,
        interrupt: AppliedInterruptCommandResult,
        identities: AmbiguousModelCallTurnIdentities,
    ) -> Result<ReconciliationRequiredModelCallTurn, ModelCallClosureError>;
    pub fn earliest_queued_turn(&self)
        -> Option<&AcceptedInputTurnSchedulingProjection>;
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
        origin_entry: SemanticTranscriptEntryId,
        starting_frontier: ContextFrontierId,
        initial_attempt: TurnAttemptId,
    ) -> Self;
    // accessors: origin_entry(), starting_frontier(), initial_attempt()
}

pub struct ActivatedAcceptedInputTurn { /* private */ }
// sealed: PreparedAcceptedInputTurnActivation or checked active scheduling projection
impl ActivatedAcceptedInputTurn {
    // accessors: session(), turn(), accepted_input(), order(), configuration(),
    // configuration_provenance(), start(), phase(), pending_steering(), consumed_steering()
}

pub struct PreparedAcceptedInputTurnActivation { /* private */ }
// sealed: AcceptedInputSchedulingProjection::prepare_earliest_queued_activation
impl PreparedAcceptedInputTurnActivation {
    pub fn into_parts(
        self,
    ) -> (
        ActivatedAcceptedInputTurn,
        SemanticTranscriptEntry,
        ResolvedContextFrontierSnapshot,
    );
    // accessors: turn(), origin_entry(), starting_snapshot(), start()
}

pub enum AcceptedInputEligibilityFailure {
    ActiveTurnPresent { turn: TurnId },
    NoQueuedTurn,
    OriginEntryIdentityAlreadyExists,
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
    // accessors: failure_entry(), terminal_frontier()
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
    );
    // accessors: turn(), failure_entry(), terminal_snapshot()
}

pub enum AcceptedInputTurnFailureFailure {
    NoActiveTurn,
    PendingSteering { accepted_input: AcceptedInputId },
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
// sealed: crate-private consuming end transitions on CurrentTurnAttempt;
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
// sealed: crate-private end transitions on CurrentModelCall; terminal —
// no transition back to a current call
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
pub struct ModelTargetCatalog { /* private */ }
pub enum ModelTargetCatalogError { DuplicateSelection { selection: DirectModelSelection } }
pub struct ResolvedModelSelection { /* private */ }
pub struct ModelTargetResolutionError { /* private */ }
pub struct ModelCallOriginContent { /* private */ }
impl ModelCallOriginContent {
    pub fn from_recorded_submit(recorded: &ReconstitutedSubmitInput) -> Option<Self>;
    pub fn from_reconstituted_turn_origin(
        origin: &SubmitInputTurnOriginReconstitutionInput,
    ) -> Option<Self>;
    // accessors: accepted_input(), content()
}

pub struct ModelCallExecutionReconstitutionInput { /* private */ }
impl ModelCallExecutionReconstitutionInput {
    pub fn new(
        active_turn: ActivatedAcceptedInputTurn,
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
    pub fn reconstitute(self) -> Result<ModelCallExecution, ModelCallExecutionReconstitutionError>;
}
pub enum ModelCallExecutionReconstitutionFailure {
    TurnIsNotRunning,
    StartingSnapshotSessionMismatch,
    StartingSnapshotMismatch,
    CallSnapshotMissing,
    CallSnapshotUnexpected,
    CallSnapshotMismatch,
    FrontierEntryMismatch,
    MultipleCalls,
    DuplicateOriginContent,
    MissingOriginContent,
    UnreferencedOriginContent,
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

pub struct ModelCallExecution { /* private */ }
impl ModelCallExecution {
    pub fn prepare_initial_call_consuming_steering(
        self,
        call: ModelCallId,
        steering_entries: Vec<SemanticTranscriptEntryId>,
        steering_frontier: Option<ContextFrontierId>,
    ) -> Result<PreparedInitialModelCall, ModelCallPreparationError>;
    pub fn recover_evidence_free_after_restart(
        self,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError>;
    pub fn apply_interrupt(
        self,
        interrupt: AppliedInterruptCommandResult,
        identities: CancelledModelCallTurnIdentities,
    ) -> Result<ModelCallInterruptOutcome, ModelCallClosureError>;
    pub fn recover_after_restart(
        self,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<ModelCallTerminalOutcome, ModelCallClosureError>;
    pub fn resume_in_flight_call(&self) -> Option<AuthorizedModelCall>;
    pub fn resume_cancellation_requested_call(&self) -> Option<StopRequestedModelCallTurn>;
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
pub enum ModelCallResumeFailure { CallMissing, CallIsNotPrepared, AttemptIsNotPrepared }
pub enum ModelCallAuthorizationFailure { CallMissing, CallIsNotPrepared, AttemptIsNotPrepared }
pub struct ModelCallAuthorizationError { /* private */ }
pub struct AuthorizedModelCall { /* private */ }
pub struct IssuedModelCallCorrelation { /* private */ }
pub struct CorrelatedModelCallTerminalObservation { /* private */ }

pub enum ModelCallTerminalObservation {
    Completed { assistant_text: Vec<AssistantText> },
    KnownFailed,
    Refused,
    Cancelled,
    Ambiguous,
}
pub struct PendingSteeringReclassificationIdentity { /* private */ }
impl PendingSteeringReclassificationIdentity {
    pub const fn new(accepted_input: AcceptedInputId, turn: TurnId) -> Self;
    // accessors: accepted_input(), turn()
}
pub struct CompletedModelCallIdentities { /* private */ }
// constructor plus with_pending_steering_reclassifications(...)
pub struct FailedModelCallTurnIdentities { /* private */ }
// constructor plus with_pending_steering_reclassifications(...)
pub struct CancelledModelCallTurnIdentities { /* private */ }
// constructor plus with_pending_steering_reclassifications(...) and into_ambiguous()
pub struct PhysicalCancellationModelCallTurnIdentities { /* private */ }
// constructor plus with_pending_steering_reclassifications(...)
pub struct RefusedModelCallTurnIdentities { /* private */ }
// constructor plus with_pending_steering_reclassifications(...)
pub struct AmbiguousModelCallTurnIdentities { /* private */ }
// constructor plus with_pending_steering_reclassifications(...)
pub enum ModelCallTerminalIdentities {
    Completed(CompletedModelCallIdentities),
    Failed(FailedModelCallTurnIdentities),
    PhysicalCancellation(PhysicalCancellationModelCallTurnIdentities),
    Refused(RefusedModelCallTurnIdentities),
    Ambiguous(AmbiguousModelCallTurnIdentities),
}
pub enum ModelCallTerminalOutcome {
    Completed(CompletedModelCallTurn),
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
}
pub struct CompletedModelCallTurn { /* private */ }
pub struct FailedModelCallTurn { /* private */ }
pub struct CancelledModelCallTurn { /* private */ }
// accessors: session(), turn(), call(), attempt(), disposition(),
// cancellation_entry(), terminal_snapshot(), reclassified_pending_steering()
pub struct StopRequestedModelCallTurn { /* private */ }
// accessors: session(), turn(), call(), attempt(), interrupt(), observation_correlation()
pub struct RefusedModelCallTurn { /* private */ }
// each terminal turn exposes reclassified_pending_steering()
pub struct ReconciliationRequiredModelCallTurn { /* private */ }
// accessors: session(), turn(), call(), attempt(), disposition(),
// terminal_snapshot(), reclassified_pending_steering()
pub struct ReclassifiedPendingSteeringTurn { /* private */ }
// sealed: successful model-call terminalization with exact pending identities
impl ReclassifiedPendingSteeringTurn {
    // accessors: session(), source_turn(), accepted_input(), turn(), order(),
    // binding(), effective_configuration()
}
pub struct AmbiguousModelCallTurn { /* private */ }
pub enum ModelCallClosureError {
    IdentityShapeMismatch,
    CallStateMismatch,
    ObservationCorrelationMismatch,
    InterruptCorrelationMismatch,
    AttemptStateMismatch,
    TargetResolutionMismatch,
    AssistantIdentityCountMismatch,
    PendingSteeringReclassificationMismatch,
    FrontierDerivationFailed,
    AmbiguityConstructionFailed,
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
    // accessors: owning_session(), snapshot(), ordered_entries()
}

pub struct ResolvedContextFrontierSnapshot { /* private */ }
// sealed: crate-private try_from_candidate and derive_appending_candidate,
// consumed by scheduling and model-call aggregate seams
impl ResolvedContextFrontierSnapshot {
    pub fn entry_count(&self) -> usize;
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

pub enum SemanticTranscriptEntryPayload {
    OriginAcceptedInput { accepted_input: AcceptedInputId },
    SteeringAcceptedInput {
        accepted_input: AcceptedInputId,
        source_turn: TurnId,
    },
    TurnFailed { turn: TurnId },
    AssistantText { producing_call: ModelCallId, value: AssistantText },
    AssistantToolUse { producing_call: ModelCallId, request: ToolRequestId },
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
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        expected_current_version: SessionConfigurationDefaultsVersion,
        replacement: SessionConfigurationDefaults,
    ) -> Self;
    pub const fn prepare_session_not_found(self) -> PreparedReplaceSessionDefaults;
    pub fn prepare_against(self, current: &Session)
        -> Result<PreparedReplaceSessionDefaults, ReplaceSessionDefaultsPreparationError>;
    // accessors: command_id(), session(), expected_current_version(), replacement()
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
    pub const fn into_parts(self) -> (ReplaceSessionDefaults, ReplaceSessionDefaultsResult);
    // accessors: command(), result()
}

pub struct ReplaceSessionDefaultsPreparationError { /* private */ }
// sealed: Err of prepare_against; adapter correlation failure, not a
// terminal command rejection
impl ReplaceSessionDefaultsPreparationError {
    pub const fn into_parts(self) -> (ReplaceSessionDefaults, SessionId);
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
    // accessors: command_id(), initial_configuration_defaults()
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

## application: model_execution

```rust
pub struct ModelCallCredentialReference { /* private */ }
impl ModelCallCredentialReference {
    pub fn new(value: impl Into<String>) -> Self;
    pub fn as_str(&self) -> &str;
}

pub enum ModelConversationMessage {
    User {
        source: SemanticTranscriptEntryRef,
        accepted_input: AcceptedInputId,
        content: UserContent,
    },
    Assistant {
        source: SemanticTranscriptEntryRef,
        producing_call: ModelCallId,
        content: AssistantText,
    },
}

pub struct PreparedModelOperation { /* private */ }
impl PreparedModelOperation {
    // accessors: request(), credential_reference(), messages()
}

pub enum ModelFrontierRenderingError {
    MissingOriginContent {
        entry: SemanticTranscriptEntryRef,
        accepted_input: AcceptedInputId,
    },
    UnsupportedAssistantToolUse { entry: SemanticTranscriptEntryRef },
}
// impl Display + std::error::Error + ClassifyOperatorFailure

pub enum PrepareModelCallOutcome {
    NoWork,
    Checkpointed(ModelCallId),
    Ready {
        request: Box<PreparedModelCallRequest>,
        credential_reference: ModelCallCredentialReference,
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
        identities: FailedModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<FailedModelCallTurn, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;
    fn reread_failure(
        &mut self,
        session: SessionId,
        call: ModelCallId,
    ) -> impl Future<Output = Result<RetainedCapabilityFailureStatus, Self::Error>> + Send;
}

pub enum RetainedCapabilityFailureStatus {
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

pub trait CommitModelCallObservationTransaction {
    type Error: ClassifyOperatorFailure;
    fn commit_observation<NextTurn>(
        &mut self,
        session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentities,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<ModelCallTerminalOutcome, Self::Error>> + Send
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
}

pub struct RetainedModelCallExecutionState { /* private */ }

pub enum ModelCallCapabilityPreparation<Capability> {
    Ready(Capability),
    Cancelled,
    KnownFailure,
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
    Checkpointed(ModelCallId),
    TargetUnavailable(Box<FailedModelCallTurn>),
    PendingSteering { accepted_input: AcceptedInputId },
    CapabilityKnownFailure(Box<FailedModelCallTurn>),
    CapabilityFailureAlreadyCommitted(ModelCallId),
    ObservationCommitted(Box<ModelCallTerminalOutcome>),
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
    CapabilityFailureCommit(FailureError),
    CapabilityFailureReread(FailureError),
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
    pub const fn new(
        ids: Ids,
        prepare: Prepare,
        failure: Failure,
        authorization: Authorization,
        observation: Observation,
        provider: Provider,
        gate: Gate,
    ) -> Self;
    pub const fn from_parts(
        ids: Ids,
        prepare: Prepare,
        failure: Failure,
        authorization: Authorization,
        observation: Observation,
        provider: Provider,
        gate: Gate,
        retained_state: Option<RetainedModelCallExecutionState>,
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
        Option<RetainedModelCallExecutionState>,
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
    // last_prepared_messages()
}
// impl ModelCallProvider
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
    ) -> Result<Self, InvalidDurableCommandId>;
    // accessors: command_id(), session(), expected_current_version(), replacement()
}

pub enum ReplaceSessionDefaultsOutcome {
    Recorded(ReplaceSessionDefaultsResult),
    ConflictingReuse { command_id: DurableCommandId },
}

pub trait ReplaceSessionDefaultsTransaction {
    type Error;

    fn handle(
        &mut self,
        command: ReplaceSessionDefaults,
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
}
```

## application: start_eligible_turn

```rust
pub trait StartEligibleTurnIdGenerator {
    fn next_origin_entry_id(&mut self) -> SemanticTranscriptEntryId;
    fn next_starting_frontier_id(&mut self) -> ContextFrontierId;
    fn next_initial_attempt_id(&mut self) -> TurnAttemptId;
}

pub struct UuidV7StartEligibleTurnIdGenerator;
// Default; impl StartEligibleTurnIdGenerator

pub enum StartEligibleTurnOutcome {
    NoEligibleTurn,
    Activated(Box<ActivatedAcceptedInputTurn>),
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
}
```

## application: scheduler

```rust
pub struct ReconciliationSweepInterval(/* private */);
impl ReconciliationSweepInterval {
    pub const fn baseline() -> Self;
    pub fn try_new(
        interval: Duration,
    ) -> Result<Self, InvalidReconciliationSweepInterval>;
    pub const fn get(self) -> Duration;
}

pub struct InvalidReconciliationSweepInterval;
// impl Display + std::error::Error

pub enum EligibilityNudgeOutcome {
    Enqueued,
    DroppedAtCapacity,
    WorkSourceClosed,
}

pub trait EligibilityNudge {
    fn nudge(&self, session: SessionId) -> EligibilityNudgeOutcome;
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
    pub fn into_parts(self) -> (Vec<SessionId>, bool);
}

pub trait EligibilityWorkSource {
    type Error;

    fn next(&mut self) -> impl Future<Output = Result<SessionId, Self::Error>> + Send;
}

pub trait EligibilityPass {
    type Error;

    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;
}

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
        sweep_interval: ReconciliationSweepInterval,
        nudge_buffer_capacity: NonZeroUsize,
    ) -> (InProcessEligibilityNudge, Self);
}

pub enum SchedulerLoopExit {
    Shutdown,
}

pub struct SchedulerLoop<WorkSource, Pass> { /* private */ }
impl<WorkSource, Pass> SchedulerLoop<WorkSource, Pass> {
    pub const fn new(work_source: WorkSource, pass: Pass) -> Self;
    pub const fn with_max_in_flight(
        work_source: WorkSource,
        pass: Pass,
        max_in_flight_passes: NonZeroUsize,
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
    fn next_reclassified_turn_id(&mut self, accepted_input: AcceptedInputId) -> TurnId;
}

pub struct UuidV7StartupScanIdGenerator;
// Default; impl StartupScanIdGenerator

pub enum StartupScanSessionOutcome {
    NoActiveTurn,
    Recovered(Box<FailedAcceptedInputTurn>),
    RecoveredModelCall(Box<ModelCallTerminalOutcome>),
    DeferredPendingSteering { accepted_input: AcceptedInputId },
}

pub trait StartupScanRepository {
    type Error: ClassifyOperatorFailure;

    fn active_sessions(
        &mut self,
    ) -> impl Future<Output = Result<Box<[SessionId]>, Self::Error>> + Send;
    fn recover<NextTurn>(
        &mut self,
        session: SessionId,
        identities: AcceptedInputTurnFailureIdentities,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<StartupScanSessionOutcome, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;
}

pub struct StartupScanOutcome { /* private */ }
// sealed: StartupScanService::execute
impl StartupScanOutcome {
    // accessors: recovered_turn_count(), pending_steering_sessions(),
    // is_complete()
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
    pub const MAX_CONTENT_UTF8_BYTES: usize; // 1_048_576
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        delivery: DeliveryRequest,
    ) -> Result<Self, SubmitInputRequestError>;
    // accessors: command_id(), session(), content(), delivery()
}

pub trait SubmitInputIdGenerator {
    fn next_accepted_input_id(&mut self) -> AcceptedInputId;
    fn next_turn_id(&mut self) -> TurnId;
    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId;
    fn next_context_frontier_id(&mut self) -> ContextFrontierId;
}

pub struct UuidV7SubmitInputIdGenerator;  // Default; impl SubmitInputIdGenerator

pub enum SubmitInputOutcome {
    Recorded(SubmitInputResult),
    ConflictingReuse { command_id: DurableCommandId },
}

pub trait SubmitInputTransaction {
    type Error;

    fn handle<NextTurn>(
        &mut self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> impl Future<Output = Result<SubmitInputOutcome, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;
}

pub struct SubmitInputService<Generator, Transaction, Nudge> { /* private */ }
impl<Generator, Transaction, Nudge> SubmitInputService<Generator, Transaction, Nudge> {
    pub const fn new(ids: Generator, transaction: Transaction, nudge: Nudge) -> Self;
    pub fn into_parts(self) -> (Generator, Transaction, Nudge);
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

## Inventory

| Module                                | Public types         |
| ------------------------------------- | -------------------- |
| domain: lib.rs identities             | 11                   |
| domain: actor                         | 1                    |
| domain: imported_conversation         | 28                   |
| domain: session                       | 18                   |
| domain: configuration                 | 19                   |
| domain: accepted_input                | 5                    |
| domain: delivery_request              | 2                    |
| domain: user_content                  | 4                    |
| domain: submit_input                  | 15                   |
| domain: queue_order                   | 5 (+1 free fn)       |
| domain: turn_lifecycle                | 10                   |
| domain: turn_eligibility              | 27                   |
| domain: turn_attempt                  | 13                   |
| domain: model_call                    | 12                   |
| domain: model_execution               | 41                   |
| domain: context_frontier              | 6                    |
| domain: semantic_entry                | 4                    |
| domain: provider_evidence             | 5                    |
| domain: applied_interrupt             | 2                    |
| domain: fatal_mismatch                | 0                    |
| domain: replace_session_defaults      | 13                   |
| **signalbox-domain total**            | **241 (+1 free fn)** |
| application: conversation_import      | 8 (incl. 3 traits)   |
| application: create_session           | 8 (incl. 2 traits)   |
| application: load_session             | 2 (incl. 1 trait)    |
| application: model_execution          | 28 (incl. 7 traits)  |
| application: operator_failure         | 2 (incl. 1 trait)    |
| application: replace_session_defaults | 4 (incl. 1 trait)    |
| application: scheduler                | 12 (incl. 4 traits)  |
| application: start_eligible_turn      | 5 (incl. 2 traits)   |
| application: startup_scan             | 7 (incl. 2 traits)   |
| application: submit_input             | 7 (incl. 2 traits)   |
| **signalbox-application total**       | **83**               |
