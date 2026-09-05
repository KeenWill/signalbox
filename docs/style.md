# House style: names over positions, provenance over literals

These rules apply to new and modified code; apply them to existing code only
when already changing it for another reason. Testing-style rules are cited as
TS-*n* and not restated.

## Scope

[`docs/agents/testing-style.md`](agents/testing-style.md) covers how a test body
reads: fixtures, assertions, snapshots, helpers. This guide covers two rules
that apply to production and test code alike:

1. **Literal provenance.** Every value visible at a use site either matters
   exactly (load-bearing) or merely needs to exist (arbitrary). The reader must
   be able to tell which without reading elsewhere.
2. **Label discipline.** Position may carry meaning only where types already do.
   A run of same-typed positions, booleans especially, becomes labeled
   structure.

Each worked example in the appendix quotes an excerpt from this repository's
history, with the line citation of its source; the rewrite after each excerpt is
the form this guide asks for.

## Core principles

### 1. Every visible literal is load-bearing

A literal spelled at a use site is a claim: *this exact value matters here*.
Values that only need to exist — an ID that must merely be distinct, a string
that must merely be non-empty — live behind names that say so: a constant, a
fixture parameter whose role the fixture's own item-level doc comment states, or
a generator.

```text
// Ambiguous: is 7 the point of this test, or just "some id"?
record_evidence(identity(7), call(2));

// Provenance named at the site:
const TARGET_IDENTITY: u128 = 7;   // the pinned identity under test
const SUBJECT_CALL: u128 = 2;      // arbitrary; only needs to be one call
record_evidence(identity(TARGET_IDENTITY), call(SUBJECT_CALL));
```

Why: an unlabeled literal costs a re-read on every future visit — the reader
must reconstruct from context whether changing it would change the test's
meaning. A name pays that cost once, at writing time. The discipline is not "no
literals", it is "no literals of unknown provenance."

### 2. Position may carry meaning only where types do

Tuples and positional arguments are fine when the element types make positions
self-evident: `(SessionId, TurnId)` cannot be read backwards, and the compiler
rejects a transposition. A run of same-typed positions — `(i64, i64, i64)`,
`(bool, bool, bool)`, three `u128` seeds in a row — forces the reader to
cross-reference positions against a definition somewhere else, and lets a
transposition compile. Such runs become labeled structure: a struct with named
fields, an enum per axis, or distinct newtypes.

```text
// The reader must count commas and consult the definition:
assert_eq!(outcome, (1, 0, 0, true));

// Each value is labeled:
assert_eq!(outcome, Backfill { scheduler_rows: 1, transcripts: 0,
                               attempts: 0, active_turn_missing: true });
```

Why: labels move the cross-referencing from every read site to one definition
site, and turn silent transposition bugs into compile errors or visibly wrong
field names.

### 3. A boolean axis becomes a two-variant enum

`true` at a call site, in a tuple, or in a match arm tells the reader nothing
about what was asked. Replace each boolean axis with a two-variant enum named
for the axis, except a private local boolean inside a function body; when the
yes-case has data, carry it in the variant. This is the "boolean blindness" fix:
keep the evidence, not just the bit. A boolean that must remain (a struct field
mirroring an external schema, say) gets a positive, assertive name:
`overflowed`, not `no_overflow`.

```text
decode(input, true);                       // true... what?
decode(input, StopSequences::Declared);    // the call site names what was asked
```

Why: a bool's meaning lives entirely in a parameter name the call site does not
show. Two-variant enums cost one `enum` line and make every site — call, match,
and failure output — self-describing.

### 4. Tests are documentation, and read as such

TS-1, TS-3, and TS-20 state how tests document the system. This guide adds: a
five-line local struct or enum defined inside a test module is cheap and
documents the test as well as a well-named test function does.

### 5. Make illegal states unrepresentable before documenting legal ones

When prose, a comment, or a test must explain which combinations of fields are
legal — "if `interrupting` is true, `predecessor` is always set" — first try to
make the illegal combinations unconstructible, then document whatever remains. A
`bool` plus an `Option` that must agree should be an enum with a payload; a
validation whose result is immediately forgotten is a parse that should have
produced a narrower type.

```text
// Two fields that must agree, policed by comments and tests:
struct Priority { interrupting: bool, predecessor: Option<TurnId> }

// One type that cannot disagree:
enum Priority { Ordinary, InterruptImmediatelyAfter { predecessor: TurnId } }
```

Why: a constraint held by the type system is enforced at every construction site
forever; a constraint held by documentation is enforced only where someone
remembered to read it. This repository's domain crate already works this way —
see the exemplars below; the rule extends that standard to test fixtures and
helper signatures, where it is applied least consistently.

## Numeric bounds

A production numeric bound stays in code only when removing it can break the
process itself. Declare that structural **guard** with
`// numeric-bound: guard - <pathological case prevented>`. A mechanically
derived guard declares the guard it derives from. Configuration may lower a
guard but not raise it.

Every other timeout, interval, attempt budget, concurrency or page bound, and
retained-detail policy the task names is required deployment configuration, with
no code default and no production constant; one the task does not name is not
introduced at all ([minimum mechanism](../AGENTS.md)). Test-only constants may
size or bound their fixture without a production declaration.

A constant whose name reads like a bound but states a fixed representation fact
— a numeric type's exact maximum, UTF-8's continuation width, the basis points
in full scale — is **not a bound**, and says so in place of a kind. Declaring it
keeps the exception visible where silence would read as an undeclared cap.

The test is whether the number could sensibly have been chosen differently. A
representation fact has one correct value that no deployment raises or lowers; a
number anyone could argue about is either a structural guard or deployment
policy however its name reads. Checking live input against a representation fact
does not convert it into a bound, and no amount of arithmetic converts a chosen
allowance into a fact.

## Conventions at component seams

Narrower rules where representations, errors, or durable data cross a boundary.

### Distinguish the user from the wire role

The human principal is the **user**. Prose does not say bare “user message,”
because a wire-role `user` message may come from a parent agent, an imported
transcript, or another non-human source. Say **user-role message** for the wire
role, or **a message from the user** for the human principal.

### One owner for bounds and durable spellings

A validation guard or numeric bound used by more than one constructor has one
named constant or checking function. A bound admitted by the process protocol is
a public constant in the protocol crate; consumers import it rather than
restating its literal.

Each closed discriminator written to PostgreSQL has one encoder and one decoder
in `crates/persistence/src/mapping.rs`. A module lifts an unknown spelling into
its own typed corruption error, but does not define another spelling table.

Every schema-evolution threshold is distinct from the version a module writes.
Name the first version that admits the feature and compare stored versions only
against that threshold. Advancing a writer version must not reinterpret rows
written under an older feature threshold.

A name has one spelling per file. A type or constant the file imports is not
also written as a crate-qualified path in a signature, field, or body; the two
spellings present one item as two, and a reader comparing sibling signatures
cannot tell whether they name the same thing. Spell both ways only to
disambiguate two same-named items from different crates, which is the one case
where the qualification carries the meaning.

A durable command identity, or any other value a verb must print for the user to
replay it, is printed through the one shared helper that produces it. A verb
that inlines the generation and the printing can forget the second half, and the
user loses the identity silently.

### Rows carry labels and failed reads stay typed

Decode a SQL projection into a named `FromRow` record with aliased columns, or
read it field by field with `try_get`. Do not decode a production row as an
anonymous tuple, including through a tuple alias or `query_as::<_, (...)>`.
Where a projected boolean represents a closed axis, decode it into an enum named
for that axis.

`sqlx::Row::get` and `get_unchecked` are forbidden. A malformed row is a typed,
fail-closed persistence error, not a panic in an open transaction.

A persistence corruption variant remains closed and matchable: it may carry a
static field or relationship label and, when needed, the observed durable
discriminator. Do not construct corruption classifications from free-form
formatted prose.

No code under `apps/` writes SQL that names a database table. Table access goes
through a method on a persistence repository or store that returns typed values
— domain enums and newtypes, not a `String` compared against a state literal.
Advisory-lock and connection-level statements that name no table are outside the
rule, as is the `signalbox-debug` diagnostic binary. A schema rename must break
the persistence crate's own tests, not the daemon at runtime.

### Durable and wire shapes change with their registries

Migration filenames sort identically under lexical and numeric ordering, and the
ordering prefix is chosen at merge time, not at authoring time — the prefix is
load-bearing policy, because a file that sorts earlier than the definition it
replaces silently re-issues the older text.

Every `command_kind` spelling a durable `CHECK` constraint admits gains its
registry variant, its join, and its presence entry in the same commit as the
migration that admits it. Consumers match the registry exhaustively, so the
first step is the only one a rule has to force.

Where a wire type declares `Serialize` and `Deserialize` separately — a `Raw*`
shadow type or a hand-written impl — every variant and every optional member is
covered by a byte-pinned round-trip test. Nothing else links the two
declarations, so a field-name divergence in a common state otherwise ships
silently. A canonical-form or version rule enforced by a SQL `CHECK` on durable
text is likewise pinned by a test that writes the Rust producer's output for
that rule through the real constraint. A rule that exists on only one side of
that boundary does not ship.

A wire type's fields stay private behind a checking constructor and accessors
whenever any doc comment on the type states an invariant — positivity,
nonemptiness, a unit, or a correlation between members. A `pub` field is
admissible only for a member whose full range is legal, and a doc comment
asserting a rule names the code that enforces it.

### Owned enums are exhaustive

A decision over a project-owned enum enumerates every variant. Do not use a
wildcard arm, a chain of `if let`, a tuple `matches!` expression with an
implicit fallthrough, or a hand-maintained variant inventory as the source of
truth. Collapsed outcomes use or-patterns; repeated decisions belong behind one
named, exhaustive accessor. Wildcards remain appropriate for foreign enums and
for non-enum or structurally open scrutinees.

### Absence is a shape, not a sentinel

"This fact is absent" is not an in-band value of a scalar that has real
meanings: no sentinel HTTP status, index, or identifier. Use `Option<T>`, or
split the function so neither half has to represent the empty case.

A boolean flag does not restate the presence of an `Option` beside it — a
`truncated` flag next to a `next_cursor`, an in-flight flag next to the
timestamp that would prove it. Model the two states as one enum, so a
constructor cannot be handed a contradictory pair and no agreement check is
needed at runtime. This is principle 5 at a seam rather than inside a module.

### Signatures carry labels, not runs

Two adjacent parameters of one function do not share a type. Group them into a
struct with named fields or give them distinct newtypes, so a transposition
cannot compile, and do so in every case where the parameters carry different
sanitization, provenance, or trust contracts. This extends principle 2 from
tuples and test fixtures to production signatures, where a transposition is most
costly and the parameter names are invisible at the call site. An
`#[allow(clippy::too_many_arguments)]` is a signal the rule is being broken, not
a licence to break it.

`bool` does not appear as a public struct field or a public function parameter
in the domain and application crates. Express each such axis as a two-variant
enum named for the question it answers, carrying the yes-case's evidence when it
has any. The rule applies to exactly the sites where the parameter name is
invisible to the caller; the private local booleans those crates use
idiomatically are untouched.

### Bodies stay small enough to read

No function body exceeds 400 lines. When a body accumulates shared mutable state
across several match arms or loop bodies, that state becomes a named struct and
each arm or phase becomes a function taking `&mut` that struct.

### Shared machinery has one home

Where two or more crates implement the same named role — provider adapters,
source-format importers, transport clients — a helper that is byte-identical, or
differs only in provider names and diagnostic strings, lives in the shared crate
they both depend on. A change that would add such a helper to a second sibling
moves it to the shared crate instead, in the same change.

Code in a provider-adapter crate names at least one of that provider's own wire
types. Machinery that operates only on the shared runtime's types — evidence
redaction, transport-error classification, bounded body collection, base-URL
validation, client construction, cancellation racing — belongs in the shared
crate, not copied per provider.

A rule deciding whether domain values are consistent with each other belongs to
the crate owning those types and is reached through that crate's public API. A
downstream crate does not restate such a rule as a local table or predicate,
even when the upstream function is currently private: make it public instead.

Before hand-writing a scanner, framer, encoder, or truncator over bytes or text,
name the standard-library function or already-declared dependency API you
rejected, and why, in a comment or the change description. A bounded-input
framer that passes that check is a sans-IO push-based framer with
per-chunk-split tests, following the shared SSE framer, and lives beside the
constant that bounds it.

### Diagnostics retain attribution and context

Reserve protocol-error classifications for facts derived from received protocol
values; a violated local invariant is an internal error and must not attribute
the fault to the peer.

A public error type implements `Display` and `Error`. Its display distinguishes
payload-bearing variants and includes the offending value when that value is
safe to expose; `source()` forwards wrapped errors. Human-facing error rendering
includes the source chain, and I/O variants retain the path, socket, or other
resource that failed.

Do not silently erase a typed error when mapping it into a coarser runtime
outcome. Emit a safe closed discriminant immediately before the mapping. A
startup configuration rejection additionally records the sanitized reason and,
for environment input, the variable name.

A fallible domain operation returns a failure type scoped to that operation, and
the failure value retains the rejected input: the offending string for a scalar
admission, the boxed input for a reconstitution. Do not write `map_err(|_| ...)`
over a structured error; add or extend a variant that carries it. One flat enum
serving dozens of operations, with context-free variants, is what this rule
prevents.

A tracing event for session work carries `session_id`; turn-scoped work also
carries `turn_id`. A subordinate identity such as a request or model-call ID
does not replace those join keys.

### Comments and presentation labels state their source

A code comment states a constraint, rationale, or contract directly. Process
artifacts, dates, reviews, and decision-history documents are provenance for git
history, not authorities cited in source comments. Cite an owning spec by name,
or an applicable scenario or invariant identifier, without a process-rule or
section number.

When code defends against a failure the type system cannot express, such as
stack depth, timing, or resource exhaustion, its comment names the failure the
defense prevents. A helper that exists only for that defense says so in its own
documentation.

Derive a user-visible label from the state it describes. Expose the label as a
computed property on the state type; do not accept independently varying state
and label inputs.

Every bounded or unit-bearing CLI argument has an explicit value name and help
text stating the bound and unit. Optional arguments also state what omission
does. Every argument and every `ValueEnum` variant carries a doc comment, since
that comment is the text a user reads in `--help`; a flag with none ships a
blank line.

A proc-macro diagnostic is spanned on the user's tokens, not on the macro call
site, and each distinct error path has a compile-fail case with a checked
`.stderr`. A diagnostic pointing at the derive rather than at the offending
literal cannot be acted on, and the missing goldens are why mis-spanning goes
unnoticed.

### Every file and public item states what it owns

Every Rust module and every Swift file under `clients/native/Sources/` opens
with a file-level doc comment — `//!` in Rust, `///` in Swift — naming what the
file owns and, where one exists, the `docs/spec/` page that governs it. A Swift
type or decoder mirroring a wire shape additionally names the wire discriminant
it decodes, which makes a missing decoder visible by inspection.

Every public item in the domain and application crates — including enum variants
and public struct fields — carries a doc comment. `DelegationTransitionFailure`
and `DelegationTransitionError` in `crates/domain/src/session_delegation.rs` are
undocumented; document them when next changing that file. `missing_docs` stays
off until the workspace has no undocumented public items (see mechanical
enforcement).

Every arm of a tagged wire decoder in the native client names its complete
admitted field set, through the shared rejection helper or a hand-written
`init(from:)` that does the same. A synthesized `Decodable` conformance is not
an acceptable decoder for a protocol message or nested record: it discards
unadmitted members, so whether a malformed frame is rejected would depend on
which arms an author remembered.

A failed prepared command is classified in exactly one named type per command
family. View models do not inline the branches that decide whether to retain a
command identity; copies of that policy drift, and the drift is invisible
because each copy reads as complete.

## Mechanical enforcement

The workspace compiler configuration forbids unsafe code. In production code,
Clippy denies panicking convenience paths (`expect`, `panic`, `unwrap`, `todo`,
`unimplemented`, and `unreachable`); the repository configuration permits
`expect`, `panic`, and `unwrap` in test targets. Clippy also denies
`sqlx::Row::get` plus `get_unchecked` in every target. These are whole-tree
gates: CI promotes every warning to an error, so a lint is configured only when
the whole workspace passes it at `deny`.

`missing_docs` is not configured.

`clippy::wildcard_enum_match_arm` is also not configured. Review enforces
explicit matching until the workspace's remaining wildcard arms are gone and the
lint can be enabled at `deny`.

### The style-rule checker

`scripts/check_style_rules.py` checks the three conventions below, which a text
scan can decide without type resolution. It runs in CI as a blocking step. A
rule is added to it only once the tree has no violations of that rule.

| Rule  | Convention it decides                                             |
| ----- | ----------------------------------------------------------------- |
| SR-8  | no production code under `apps/` names a table in SQL             |
| SR-12 | every clap argument and `ValueEnum` variant carries a doc comment |
| SR-13 | no proc-macro diagnostic is spanned on the macro call site        |

The other conventions in this guide are applied by the author when writing the
change, because the fact they depend on (who owns a bound, whether two helpers
are the same helper, whether an error value is safe to render) is not in the
text. Do not add a blanket crate-level `allow` to silence a lint warning.

## Rust mechanics (appendix)

### A. Function-local record types in tests

A struct defined next to the tests that use it needs no ceremony: derive `Debug`
and `PartialEq`, name the fields, and both assertions and failure output become
labeled. Match with field names and `..` for the fields a given arm does not
constrain — the pattern then states exactly which facts the arm depends on.

Worked example:
`crates/persistence/tests/postgres_integration/model_call_execution_and_recovery.rs:2441-2460`
asserts a migration backfill through a six-way tuple:

```rust
let backfilled: (i64, String, i64, i64, i64, bool) = sqlx::query_as(
    "SELECT
        (SELECT count(*) FROM session_scheduler WHERE session_id = $1),
        turn.state_kind,
        (SELECT count(*) FROM semantic_transcript_entry),
        ...
        typed.result_actual_active_turn_id IS NULL
     FROM turn_lifecycle AS turn ...",
)
...
assert_eq!(backfilled, (1, "queued".to_owned(), 0, 0, 0, true));
```

Which `0` is the frontier count? What is `true`? The reader must line the tuple
up against the SELECT list by eye. With a local row struct each value is
labeled:

```rust
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct BackfillFacts {
    scheduler_rows: i64,
    turn_state: String,
    transcript_entries: i64,
    context_frontiers: i64,
    turn_attempts: i64,
    actual_active_turn_missing: bool,
}

assert_eq!(
    backfilled,
    BackfillFacts {
        scheduler_rows: 1,
        turn_state: "queued".to_owned(),
        transcript_entries: 0,
        context_frontiers: 0,
        turn_attempts: 0,
        actual_active_turn_missing: true,
    }
);
```

`FromRow` requires named columns (`AS scheduler_rows`, …), which also makes the
SQL readable. On failure, `Debug` output prints field names instead of a
positional tuple, so the assertion message improves (TS-20).

### B. A two-variant enum per boolean axis

Worked example: the streaming budget helper in both provider runtimes
(`crates/model-runtime-openai/src/runtime.rs:506`, mirrored in
`crates/model-runtime-anthropic/src/runtime.rs`) returned `(usize, bool)`, and
its tests asserted bare pairs
(`crates/model-runtime-openai/src/runtime.rs:2036-2050`):

```rust
fn streamed_response_prefix_len(current: usize, chunk: usize) -> (usize, bool) { ... }

assert_eq!(
    streamed_response_prefix_len(MAX_STREAMED_RESPONSE_BYTES - 1, 2),
    (1, true)
);
```

Only the definition knows the bool means "this chunk overflowed the budget." The
minimum fix is a named-field struct; the better fix makes the axis an enum so
the accepting and overflowing outcomes are distinct at a glance:

```rust
#[derive(Debug, PartialEq)]
enum PrefixBudget {
    Accepted { len: usize },
    Overflowed { accepted_len: usize },
}

assert_eq!(
    streamed_response_prefix_len(MAX_STREAMED_RESPONSE_BYTES - 1, 2),
    PrefixBudget::Overflowed { accepted_len: 1 }
);
```

The minimum fix —
`struct PrefixBudget { accepted_len: usize, overflowed: bool }` — labels the
fields but keeps the boolean axis; prefer the enum.

The same axis-erasure appeared at call sites:
`StreamDecoder::new(ExchangeFacts::default(), false)` (three call sites) and the
test drivers `drive_with_stop_sequences(&[...], true)` /
`decode_with_stop_sequences(json, true)`
(`crates/model-runtime-openai/src/stream.rs:594-599`,
`crates/model-runtime-openai/src/response.rs:384-390`). The parameter was
`stop_sequences_declared: bool` — a name every call site hides. As
`enum StopSequences { Declared, NotDeclared }`, every site says what it means,
and the variant is where declared sequences themselves would live if the decoder
ever needs them (principle 5).

### C. Arbitrary versus load-bearing, spelled out

Three house forms; the first two appear in the repository at the citations
below, and the third pairs TS-4's one-knob fixture with a naming convention this
guide introduces:

- **Doc-commented constants** for a test module's set of identities —
  `crates/domain/src/provider_evidence.rs:790-795` is the model:

  ```rust
  const TARGET_IDENTITY: u128 = 7;
  /// The one call most tests record evidence for; helpers that omit a call
  /// seed act on this call.
  const SUBJECT_CALL: u128 = 2;
  /// The canonical reported identity that mismatches [`TARGET_IDENTITY`].
  const MISMATCHING_IDENTITY: u128 = 8;
  ```

  The comments state each value's role: `8` must differ from `7`, and nothing
  more. `TARGET_IDENTITY` carries no comment, so whether `7` is load-bearing is
  left to the reader; under principle 1 it would say "the pinned identity under
  test; `MISMATCHING_IDENTITY` differs from `TARGET_IDENTITY`". Add that comment
  when next changing the file.

- **Namespaced generators** for values whose only obligation is distinctness —
  `next_test_submit_uuid()` in `postgres_integration/main.rs:1722-1725` prefixes
  its IDs with `0xfeed_cafe_dead_beef`. A value from a generator is arbitrary by
  construction; no reader will mistake it for load-bearing.

- **One-knob fixtures** (TS-4) so arbitrary plumbing does not reach the test
  body at all, and — where a value must appear literally but any value would do
  — an `ARBITRARY_`-prefixed constant such as `ARBITRARY_SESSION_ID`
  (`crates/runner-wire/src/tests.rs` uses the prefix).

Worked example: `checkpoint_restart_model_call` combines both problems in one
signature (`postgres_integration/main.rs:3084-3088`, twelve call sites):

```rust
async fn checkpoint_restart_model_call(pool: &PgPool, seed: u128, authorize: bool) -> ...

let prepared = checkpoint_restart_model_call(&pool, 0x2000, false).await?;
let issued   = checkpoint_restart_model_call(&pool, 0x3000, true).await?;
let stopped  = checkpoint_restart_model_call(&pool, 0x3500, true).await?;   // :5635-5637
```

The bools are blind — only the variable names hint that `true` means the call
was authorized — and the hex seeds are arbitrary (they need only avoid
collision) yet look exactly like the load-bearing hex elsewhere in the file.
After applying B and C:

```rust
#[derive(Debug, Clone, Copy)]
enum RestartCheckpoint {
    Prepared,
    Authorized,
}

// Seeds are arbitrary; each fixture in a test needs a distinct one.
// (`//`, not `///`: a doc comment on a statement trips `unused_doc_comments`.)
let prepared =
    checkpoint_restart_model_call(&pool, seed_a(), RestartCheckpoint::Prepared).await?;
let issued =
    checkpoint_restart_model_call(&pool, seed_b(), RestartCheckpoint::Authorized).await?;
```

### D. Labeled matches for flag inventories

Worked example: the registry corruption check matched a kind against six
presence booleans (`crates/persistence/src/command_registry.rs:130-170`,
production code), abridged here to three flags:

```rust
match (kind, has_create, has_defaults, has_input) {
    (CommandKind::CreateSession, true, false, false)
    | (CommandKind::ReplaceSessionDefaults, false, true, false)
    | (CommandKind::SubmitInput, false, false, true) => Ok(kind),
    (CommandKind::CreateSession, false, false, false)
    | (CommandKind::ReplaceSessionDefaults, false, false, false)
    | (CommandKind::SubmitInput, false, false, false) => Err(...MissingTypedRecord(kind)),
    _ => Err(...ConflictingTypedRecords),
}
```

Every arm demands positional cross-referencing, and the actual rule — *exactly
one typed record exists, and it is the registered kind* — is nowhere stated.
Pairing each flag with its kind makes the code state the rule (with the decision
match extracted as `sole_typed_record` so unit tests pin its arms without a
database):

```rust
let present: Vec<CommandKind> = [
    (CommandKind::CreateSession, has_create),
    (CommandKind::ReplaceSessionDefaults, has_defaults),
    (CommandKind::SubmitInput, has_input),
]
.into_iter()
.filter_map(|(candidate, is_present)| is_present.then_some(candidate))
.collect();

match present.as_slice() {
    [only] if *only == kind => Ok(kind),
    [] => Err(RegistryInspectionError::Corruption(
        RegistryCorruption::MissingTypedRecord(kind),
    )),
    _ => Err(RegistryInspectionError::Corruption(
        RegistryCorruption::ConflictingTypedRecords,
    )),
}
```

Where a match over named facts genuinely needs many axes, build a local struct
and match with field names plus `..` — an arm then names exactly the facts it
constrains and omits the rest.

### E. Baselines and struct update for perturbation tests

The `CurrentSessionFacts::matching(...)` + `..` rewrite in TS-4 is the house
pattern: a canonical baseline constructor, with each test naming only the field
it perturbs via struct-update syntax. It composes with everything above — a
baseline of *named* fields is what makes `..` meaningful. Prefer it over
positional fixture arguments anywhere two or more values share a type.

### F. Newtypes end same-typed runs

The domain crate's ID discipline (`SessionId`, `TurnId`, `AcceptedInputId`,
`DurableCommandId`, …) is why most of its signatures need no labels: positions
differ in type, so they cannot be confused (principle 2, and Rust API Guidelines
C-NEWTYPE). Extend the same discipline to counts and versions in helper
signatures. For example, `input_choices(expected: u64, ...)`
(`postgres_integration/main.rs:2619`) takes a bare `u64` immediately converted
into a `SessionConfigurationDefaultsVersion`; twenty-plus call sites read
`input_choices(1, ...)` where neither the name `expected` nor the literal `1`
reveals that the value is a defaults *version*. Take the domain type and the
call sites explain themselves. Renaming the parameter alone does not change
them: Rust shows no argument names at a call, so `expected` and
`defaults_version` both read as `input_choices(1, ...)` — a definition-site
improvement only.

## Internal exemplars

Where the repository already does this well; point reviews here.

- `crates/domain/src/queue_order.rs:54` — `AcceptedInputQueuePriority`: a
  bool-plus-option collapsed into `Ordinary` versus
  `InterruptImmediatelyAfter { predecessor }` (principles 3 and 5).
- `crates/domain/src/provider_evidence.rs:790-795` — doc-commented test
  constants distinguishing load-bearing from arbitrary identities (principle 1).
- `crates/persistence/tests/postgres_integration/main.rs:1722-1725` —
  `next_test_submit_uuid()`, arbitrary-by-construction IDs under a visible
  namespace (principle 1).
- The domain crate's ID newtypes and state enums throughout —
  `CurrentModelCallState`, `TurnDisposition`, `AttemptEnd`, … (principles 2 and
  5).
- `docs/agents/testing-style.md` rules 4-6, 16, and 20 — fixture knobs,
  fixture-based assertions, check helpers, and failure messages; this guide's
  principles 1 and 4 are their production-and-test-wide generalization.

## Sources

- [Rust API Guidelines — Type safety](https://rust-lang.github.io/api-guidelines/type-safety.html)
  — C-NEWTYPE and C-CUSTOM-TYPE: "arguments convey meaning through types, not
  `bool` or `Option`"; prefer `Widget::new(Small, Round)` to
  `Widget::new(true, false)`.
- [Robert Harper — Boolean Blindness](https://existentialtype.wordpress.com/2011/03/15/boolean-blindness/)
  — a boolean erases its own provenance; to use one you must already know what
  it means. Keep the evidence, not the bit.
- [Alexis King — Parse, don't validate](https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/)
  — encode the outcome of a check in a type, so the knowledge cannot be lost and
  re-checked.
- [Yaron Minsky — Effective ML Revisited](https://blog.janestreet.com/effective-ml-revisited/)
  — make illegal states unrepresentable; variants over field combinations that
  must agree.
- [Scott Wlaschin — Designing with types: Making illegal states unrepresentable](https://fsharpforfunandprofit.com/posts/designing-with-types-making-illegal-states-unrepresentable/)
  — the same principle worked as a refactoring recipe: replace and-ed/or-ed
  field constraints with a variant per legal shape.
- [Google Testing Blog — Tests Too DRY? Make Them DAMP!](https://testing.googleblog.com/2019/12/testing-on-toilet-tests-too-dry-make.html)
  — in tests, obviousness outranks deduplication.
- [Google Testing Blog — Don't Put Logic in Tests](https://testing.googleblog.com/2014/07/testing-on-toilet-dont-put-logic-in.html)
  — expectations are literal values, not recomputed by test-side logic.
- [Google Testing Blog — Improve Readability With Positive Booleans](https://testing.googleblog.com/2023/10/improve-readability-with-positive.html)
  — booleans that survive get positive, assertive names.
- [matklad — How to Test](https://matklad.github.io/2021/05/31/how-to-test.html)
  — data-driven check functions and "artisanally crafted" failure messages;
  already cited by TS-9 and TS-20.
