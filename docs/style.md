# House style: names over positions, provenance over literals

**Status: normative for new and modified code.** Apply these principles to
existing code only when already changing it for another reason. Rules from the
testing style guide are cited below as TS-*n*; this guide restates none of them.

## Scope

[`docs/agents/testing-style.md`](agents/testing-style.md) owns how a test body
reads — fixtures, assertions, snapshots, helpers. This guide owns two narrower
disciplines that apply to production and test code alike:

1. **Literal provenance.** Every value visible at a use site either matters
   exactly (load-bearing) or merely needs to exist (arbitrary). The reader must
   be able to tell which, without archaeology.
2. **Label discipline.** Position may carry meaning only where types already do.
   A run of same-typed positions — booleans worst of all — must become labeled
   structure.

Every worked example in the appendix starts from a real excerpt of this
repository, abridged where the text says so, and quoted (with its line
citations) as of the adoption commit; the rewrite that follows each excerpt is
proposed, not existing code, except where the text says it has since landed.

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
meaning. A name pays that cost once, at writing time. Corollary: when a test
*does* care about a value, spell the literal at the assertion (TS-5) — or, where
the expected value is one the fixture already states, assert against the fixture
itself (TS-6). The discipline is not "no literals", it is "no literals of
unknown provenance."

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

// The labels travel with the values:
assert_eq!(outcome, Backfill { scheduler_rows: 1, transcripts: 0,
                               attempts: 0, active_turn_missing: true });
```

Why: labels move the cross-referencing from every read site to one definition
site, and turn silent transposition bugs into compile errors or visibly wrong
field names.

### 3. A boolean is an answer with its question erased

`true` at a call site, in a tuple, or in a match arm tells the reader nothing
about what was asked. Replace each boolean axis with a two-variant enum named
for the axis; when the yes-case has data, carry it in the variant. This is the
"boolean blindness" fix: keep the evidence, not just the bit. A boolean that
must remain (a struct field mirroring an external schema, say) gets a positive,
assertive name — `overflowed`, never `no_overflow`.

```text
decode(input, true);                       // true... what?
decode(input, StopSequences::Declared);    // the question travels with the answer
```

Why: a bool's meaning lives entirely in a parameter name the call site never
shows. Two-variant enums cost one `enum` line and make every site — call, match,
and failure output — self-describing.

### 4. Tests are documentation, and read as such

The test suite is where a maintainer learns what the system promises. TS-1,
TS-3, and TS-20 set that standard and remain its only statement; read them
there. This guide adds the label discipline to it: defining a five-line local
struct or enum inside a test module is cheap, and it is exactly as legitimate a
documentation move as a well-named test function.

### 5. Make illegal states unrepresentable before documenting legal ones

When prose, a comment, or a test must explain which combinations of fields are
legal — "if `interrupting` is true, `predecessor` is always set" — first try to
make the illegal combinations unconstructible, then document whatever remains. A
`bool` plus an `Option` that must agree is an enum with a payload waiting to be
written; a validation whose result is immediately forgotten is a parse that
should have produced a narrower type.

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
helper signatures, where it is applied least consistently today.

## Conventions at component seams

The core principles imply the following narrower rules where representations,
errors, or durable data cross a boundary. These rules are normative for new and
modified code in their stated scope.

### Distinguish the user from the wire role

The human principal is the **user**. Prose never says bare “user message,”
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

### Rows carry labels and failed reads stay typed

Decode a SQL projection into a named `FromRow` record with aliased columns, or
read it field by field with `try_get`. Do not decode a production row as an
anonymous tuple, including through a tuple alias or `query_as::<_, (...)>`.
Where a projected boolean represents a closed axis, decode it into an enum named
for that axis.

`sqlx::Row::get` and `get_unchecked` are forbidden. A malformed row is a typed,
fail-closed persistence error, never a panic in an open transaction.

A persistence corruption variant remains closed and matchable: it may carry a
static field or relationship label and, when needed, the observed durable
discriminator. Do not construct corruption classifications from free-form
formatted prose.

### Owned enums are exhaustive

A decision over a project-owned enum enumerates every variant. Do not use a
wildcard arm, a chain of `if let`, a tuple `matches!` expression with an
implicit fallthrough, or a hand-maintained variant inventory as the source of
truth. Collapsed outcomes use or-patterns; repeated decisions belong behind one
named, exhaustive accessor. Wildcards remain appropriate for foreign enums and
for non-enum or structurally open scrutinees.

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
does.

## Mechanical enforcement

The workspace compiler configuration forbids unsafe code. In production code,
Clippy denies panicking convenience paths (`expect`, `panic`, `unwrap`, `todo`,
`unimplemented`, and `unreachable`); the repository configuration permits
`expect`, `panic`, and `unwrap` in test targets. Clippy also denies
`sqlx::Row::get` plus `get_unchecked` in every target. These are whole-tree
gates: CI promotes every warning to an error, so a lint is configured only when
the whole workspace passes it at `deny`.

`missing_docs` is not configured yet. A follow-up enables it at `deny` only
after the outstanding undocumented items across the workspace are cleared.

`clippy::wildcard_enum_match_arm` is also not configured. A whole-workspace
probe reports 211 remaining violations:

- `signalbox-client`: 47; `signalbox-persistence`: 41; `signalbox-domain`: 34;
  `signalboxd`: 23; `signalbox-model-runtime-anthropic`: 15;
  `signalbox-model-runtime-openai`: 11; `signalbox-model-runtime-claude-cli`: 7;
  `signalbox-model-runtime-codex-cli`: 6; `signalbox-application`,
  `signalbox-expect-table`, and `signalbox-tools-code-host`: 5 each;
  `signalbox-conversation-import-codex`: 3; `signalbox-model-runtime`,
  `signalbox-model-provider-runtime`, and `signalbox-process-protocol`: 2 each;
  `signalbox-conversation-import-claude-code`, `signalbox-tool-contract`, and
  `signalbox-tool-schema-derive`: 1 each.

The audit-fixes effort owns active violations in `apps/client/src/`,
`apps/signalboxd/`, `crates/domain/src/model_execution.rs`, the model-runtime
crates, `crates/process-protocol/`, and `crates/tools-code-host/`; the remaining
domain and persistence matches require a separate exhaustive-match follow-up.
Review enforces explicit matching until that complete inventory reaches zero and
the lint can be enabled at `deny`.

Review continues to enforce the semantic halves that syntax alone cannot prove:
which crate owns a bound or durable spelling, whether a row record's labels are
correct, whether an enum is project-owned, whether an error value is safe to
render, whether diagnostic context is sufficient, and whether comments and
presentation labels name the fact they actually describe. A lint warning is
never treated as permission to add a blanket crate-level allowance.

## Rust mechanics (appendix)

### A. Function-local record types in tests

A struct defined next to the tests that use it needs no ceremony: derive `Debug`
and `PartialEq`, name the fields, and both assertions and failure output become
labeled. Match with field names and `..` for the fields a given arm does not
care about — the pattern then states exactly which facts the arm depends on.

**Worked example** —
`crates/persistence/tests/postgres_integration.rs:6633-6652` asserts a migration
backfill through a six-way tuple:

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
up against the SELECT list by eye. With a local row struct the labels travel:

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

`FromRow` requires named columns (`AS scheduler_rows`, …), which repairs the
SQL's readability in the same stroke. On failure, `Debug` output prints field
names instead of a positional tuple — the assertion message improves for free
(TS-20).

### B. A two-variant enum per boolean axis

**Worked example** — the streaming budget helper in both provider runtimes
(`crates/model-runtime-openai/src/runtime.rs:506`, mirrored in
`crates/model-runtime-anthropic/src/runtime.rs`) returned `(usize, bool)`, and
its tests asserted bare pairs
(`crates/model-runtime-openai/src/runtime.rs:2036-2050`); the enum rewrite below
has since landed in both runtimes:

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
`enum StopSequences { Declared, NotDeclared }` — since landed as the decoder's
actual signature — every site says what it means, and the variant is where
declared sequences themselves would live if the decoder ever needs them
(principle 5).

### C. Arbitrary versus load-bearing, spelled out

Three house forms; the first two appear in the repository at the citations
below, and the third pairs TS-4's one-knob fixture with a naming convention this
guide introduces:

- **Doc-commented constants** for a test module's cast of identities —
  `crates/domain/src/provider_evidence.rs:790-795` is the model:

  ```rust
  const TARGET_IDENTITY: u128 = 7;
  /// The one call most tests record evidence for; helpers that omit a call
  /// seed act on this call.
  const SUBJECT_CALL: u128 = 2;
  /// The canonical reported identity that mismatches [`TARGET_IDENTITY`].
  const MISMATCHING_IDENTITY: u128 = 8;
  ```

  The comments state each value's *role*; the reader never wonders whether `8`
  is meaningful (it is: it must differ from `7`, and nothing more). The excerpt
  is faithful, gap included: `TARGET_IDENTITY` carries no comment, so whether
  `7` is itself load-bearing is left to the reader — under principle 1 it would
  say *the pinned identity under test; any value `MISMATCHING_IDENTITY` differs
  from serves*. This guide reaches that file the next time it is modified.

- **Namespaced generators** for values whose only obligation is distinctness —
  `next_test_submit_uuid()` in `postgres_integration.rs:129-132` brands its IDs
  with a `0xfeed_cafe_dead_beef` prefix. A value from a generator is arbitrary
  by construction; no reader will mistake it for load-bearing.

- **One-knob fixtures** (TS-4) so arbitrary plumbing never reaches the test body
  at all, and — where a value must appear literally but any value would do — an
  `ARBITRARY_`-prefixed constant such as `ARBITRARY_SESSION_ID`. The prefix is a
  convention this guide introduces; no constant in the tree carries it yet, so
  the first use establishes it.

**Worked example** — `checkpoint_restart_model_call` combines both problems in
one signature (`postgres_integration.rs:1037-1041`, twelve call sites):

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

**Worked example** — the registry corruption check matched a kind against six
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
Pairing each flag with its kind makes the rule the code, the form that has since
landed there (with the decision match extracted as `sole_typed_record` so unit
tests pin its arms without a database):

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
constrains and stays silent about the rest.

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
(`postgres_integration.rs:663`) takes a bare `u64` immediately converted into a
`SessionConfigurationDefaultsVersion`; twenty-plus call sites read
`input_choices(1, ...)` where neither the name `expected` nor the literal `1`
reveals that the value is a defaults *version*. Take the domain type and the
call sites explain themselves. Renaming the parameter alone does not reach them:
Rust shows no argument names at a call, so `expected` and `defaults_version`
both read as `input_choices(1, ...)` — a definition-site improvement only.

## Internal exemplars

Where the repository already does this well; point reviews here.

- `crates/domain/src/queue_order.rs:54` — `AcceptedInputQueuePriority`: a
  bool-plus-option collapsed into `Ordinary` versus
  `InterruptImmediatelyAfter { predecessor }` (principles 3 and 5).
- `crates/domain/src/provider_evidence.rs:790-795` — doc-commented test
  constants distinguishing load-bearing from arbitrary identities (principle 1).
- `crates/persistence/tests/postgres_integration.rs:129-132` —
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
  — expectations are literal values, never recomputed by test-side logic.
- [Google Testing Blog — Improve Readability With Positive Booleans](https://testing.googleblog.com/2023/10/improve-readability-with-positive.html)
  — booleans that survive get positive, assertive names.
- [matklad — How to Test](https://matklad.github.io/2021/05/31/how-to-test.html)
  — data-driven check functions and "artisanally crafted" failure messages;
  already cited by TS-9 and TS-20.
