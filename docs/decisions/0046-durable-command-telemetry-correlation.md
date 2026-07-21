# ADR-0046: Durable-command telemetry correlation

- Date: 2026-07-20
- Supersedes: [ADR-0044](0044-hub-runtime-foundations.md)'s telemetry
  correlation clause for caller-supplied `DurableCommandId` values
- Superseded by: none
- Depends on: [ADR-0001](0001-domain-terminology-and-identity.md),
  [ADR-0017](0017-credential-lifecycle.md),
  [ADR-0032](0032-postgres-implementation-dependencies.md),
  [ADR-0033](0033-identity-generation-supply-and-encoding.md), and
  [ADR-0044](0044-hub-runtime-foundations.md)
- Decision questions: derivation and encoding of a safe command-correlation
  token; key ownership and delivery; stability, rotation, and historical
  correlation; redaction and failure behavior

## Context

[ADR-0044](0044-hub-runtime-foundations.md)'s original telemetry clause permits
raw durable-command identities as named correlation keys even when callers
supply them. This record changes and supersedes that permission: a raw
caller-supplied `DurableCommandId` is prohibited from operational telemetry and
must be represented by a stable, non-reversible correlation token.
[ADR-0033](0033-identity-generation-supply-and-encoding.md) accepts every
non-sentinel UUID from a caller, including predictable values, so an unkeyed
digest can be reversed by enumeration. A process-random key prevents enumeration
but loses correlation after restart.

The missing choice is foundation-weight: it introduces durable secret material
at the runtime/observability boundary and decides what stability means during
rotation. It must not turn telemetry into domain, storage, or protocol
authority; expose caller bytes; or broaden
[ADR-0017](0017-credential-lifecycle.md)'s provider-and-integration credential
access port.

## Decision

### One versioned, domain-separated token

The hub observability boundary represents a caller-supplied `DurableCommandId`
with this ASCII token:

```text
dc1.<key-id>.<digest>
```

`key-id` is a deployment-assigned, non-secret identifier for one key epoch. It
contains only lowercase ASCII letters, digits, and hyphens, is 1–32 bytes, and
is never reused for different key bytes. `digest` is unpadded base64url of the
first 16 bytes of:

```text
HMAC-SHA-256(key, "signalbox/durable-command-telemetry/v1\0" || uuid-bytes)
```

`uuid-bytes` is exactly the RFC 9562 canonical 16-octet UUID sequence: remove
the four hyphens from the lowercase canonical 8-4-4-4-12 hexadecimal text and,
from left to right, decode each consecutive hexadecimal pair as one octet. The
first text pair is octet 0 and the last is octet 15; mixed-endian GUID field
layouts are prohibited. The literal domain separator, NUL byte, HMAC algorithm,
truncation, and encoding are part of version `dc1`; changing any of them
requires a new token version. Implementations use a reviewed HMAC-SHA-256
implementation, not a repository-owned cryptographic primitive. This record adds
no dependency by itself; a future implementation pull request may select a
focused cryptographic crate under the repository dependency rules.

The 128-bit output is a telemetry label only. It is not a domain identity,
durable-command proof, storage key, protocol field, authorization fact, or
deduplication key. Equality between tokens is meaningful only when both token
version and `key-id` match.

### Deployment owns and delivers the key epoch

The Signalbox deployment owner creates each 32-byte uniformly random HMAC key,
assigns its unique `key-id`, and owns its source of truth, backup, access, and
retirement. This key is an observability secret, not a provider or integration
credential; it does not enter ADR-0017's credential-reference mapping or
credential access port.

The deployment supplies the active key and its `key-id` to hubd together in one
read-only, volume-mounted epoch document. One open and complete read must yield
both values from the same atomic secret projection; independently mounted or
independently read key and identifier files are prohibited. The serialization
format is not fixed here, but it must be unambiguous and reject missing,
duplicate, or trailing fields. Neither value arrives in command-line arguments,
ordinary configuration files, process environment variables, Postgres, domain
values, or protocol messages. The secret key never appears in telemetry. The
non-secret `key-id` may appear only inside the complete token or in a dedicated
structured correlation-key-ID field; free-form messages do not interpolate it.
Only hubd's correlation-token component reads the epoch document. Library code
receives an opaque token-derivation capability, never key bytes. The upstream
secret manager and Kubernetes manifest shape remain deployment mechanics,
preserving [ADR-0017](0017-credential-lifecycle.md)'s ownership of its
credential classes and
[ADR-0032](0032-postgres-implementation-dependencies.md)'s open
database-credential delivery question.

Hubd opens, completely reads, and validates exactly one epoch document during
startup, then keeps the resulting epoch for that process lifetime. Updating the
mounted document does not change a running process's epoch. Rotation activates
only through a deliberate hub restart or rollout, so one process never emits two
tokens for the same command because a mount changed between reads.

### Stability is explicit across restart and rotation

Within one key epoch, a command produces the same token across processes,
restarts, replicas, and deployments that receive the same key and `key-id`. This
is the stability [ADR-0044](0044-hub-runtime-foundations.md) requires for
ordinary durable-failure correlation.

Rotation starts a new epoch with fresh key bytes and a never-reused `key-id`.
The same command deliberately produces a different token after activation. Logs
on opposite sides of that boundary are not directly linkable by token equality.
Routine rotation retains former keys only in the deployment owner's protected
secret archive for the required log-investigation horizon. An authorized offline
diagnostic tool may derive the old epoch's token from a known command identifier
and archived key; the running hub never loads retired keys and never emits
cross-epoch aliases.

Compromise rotation destroys or quarantines the former key instead of retaining
it for correlation. Loss of cross-epoch linkage is the intentional security
cost. Rotation never rewrites historical telemetry, changes durable state, or
changes command identity and replay semantics under
[ADR-0001](0001-domain-terminology-and-identity.md).

### Redaction is fail closed

The ADR-0044 operational telemetry boundary never emits raw caller-supplied
`DurableCommandId` values, their UUID text or bytes, integer form, prefixes,
suffixes, or unkeyed digests. Structured telemetry fields use the complete token
and a name that identifies it as correlation rather than command identity.
Free-form telemetry messages do not interpolate either the raw identifier or key
material.

Existing domain, application, or persistence error `Debug` and `Display`
representations may contain a raw identifier and are therefore sensitive
internal values, not telemetry-safe renderings. Observability code translates
the typed error and derives the token from its typed identifier; it does not log
the formatted error. This record does not redefine general-purpose error
formatting outside the operational telemetry boundary.

Hubd installs a sanitized panic hook before configuration loading, migrations,
recovery, task spawning, or work admission. The hook replaces rather than chains
to the default hook, never renders the panic payload or a dynamically formatted
error, and emits only a fixed panic classification plus an optional static
source location. It must not emit a raw command identifier, user content,
credential, epoch key, or other free-form runtime value. The merged repository
panic discipline still requires expected failures to use typed errors; the hook
is the last-resort redaction boundary, not normal error reporting or recovery.

Registry-level and pre-claim events required by
[ADR-0044](0044-hub-runtime-foundations.md) use the token without inventing a
session key. Session-scoped events may include ADR-0044's permitted hub-minted
aggregate identifiers alongside it. Token derivation does not relax
[ADR-0017](0017-credential-lifecycle.md)'s credential redaction or ADR-0044's
user-content and payload redaction.

Missing, unreadable, malformed, incomplete, or non-32-byte epoch key material
fails hubd startup before migrations, recovery, scheduling, protocol admission,
or worker startup. An empty, longer-than-32-byte, or otherwise invalid `key-id`
in that same document is likewise a configuration failure. There is no fallback
to a raw identifier, unkeyed digest, process-random key, or fixed default key.

Successful startup constructs one immutable in-memory capability that owns the
validated key epoch. It performs no later file read, refresh, lookup, or other
fallible external operation: derivation is total for every valid
`DurableCommandId`, and the capability remains available for the process
lifetime. Every command-scoped corruption event therefore carries the required
token. A derivation implementation defect is a process-fatal bug handled by the
sanitized panic boundary, never an event-level omission or raw-ID fallback.

## Invariants

- INV-002: token and key remain runtime/observability representations and never
  become domain, storage, or wire representations.
- INV-012: the token neither replaces nor creates owner-global durable-command
  identity or replay authority.

## Rejected alternatives

- **Unkeyed hashing.** Predictable caller UUIDs are enumerable, so a digest is
  reversible in practice.
- **A random key per process.** This prevents correlation across replicas and
  restart, including startup recovery of earlier work.
- **One never-rotated global key.** This preserves token equality forever but
  gives compromise an unbounded historical blast radius.
- **Load all epochs and emit aliases.** Cross-epoch aliases make every online
  hub a historical-correlation oracle and multiply sensitive telemetry labels.
- **Persist a token beside each command.** That introduces a storage
  representation and pre-claim lookup problem solely for telemetry; keyed epoch
  derivation keeps observability outside durable authority.
- **Use [ADR-0017](0017-credential-lifecycle.md)'s credential port.** That port
  is keyed by provider or integration credential references at effect
  boundaries. Generalizing it to a process-wide telemetry key would blur its
  accepted scope and access rules.

## Consequences

Normal restarts and horizontally scaled replicas preserve useful correlation
when deployment supplies the same epoch. Rotation intentionally creates a
visible correlation boundary. Historical linkage requires both a known raw
identifier and authorized access to an archived key; possession of logs alone
does not enable enumeration of low-entropy UUIDs.

The hubd implementation must validate key configuration before side effects and
must centralize token formatting so adapters cannot improvise redaction. A
focused HMAC implementation dependency is likely, but this ADR neither selects
nor adds one.

## Scenario walkthroughs

- **[S01](../scenarios.md#s01--create-a-new-interactive-session) — pre-claim
  conflicting reuse:** registry lookup classifies reuse of the claimed command
  identifier under [ADR-0044](0044-hub-runtime-foundations.md) and logs
  `dc1.<key-id>.<digest>` without a session identity or raw caller UUID. Another
  replica in the same epoch emits the identical token. This maps S01's existing
  conflicting-reuse fixture; it adds no command outcome or lifecycle edge.
- **[S03](../scenarios.md#s03--hub-restarts-after-accepting-queued-work) —
  restart recovery:** the deployment supplies the same epoch after an ordinary
  restart, so recovery and earlier live-operation events correlate. This maps
  S03's existing recovery fixture without changing its durable or transient
  state.
- **Routine rotation:** a rollout activates a fresh epoch. New events use the
  new `key-id`; an authorized investigator with a known command ID may derive
  both epoch tokens offline while the running hub knows only the current key.
- **Invalid deployment:** hubd cannot completely read and validate one coherent
  mounted epoch document and exits before migrations or work admission. It never
  combines separate reads or downgrades to raw logging.

## Open questions

- Log retention and the matching archived-key retention period remain an
  operational policy decision.
- The concrete secret manager, file paths, permissions, rollout mechanism, HMAC
  crate, and offline diagnostic tool remain implementation choices constrained
  by this contract.

## Explicit non-decisions

This record does not select a logging subscriber, secret manager, cryptographic
crate, storage schema, wire field, authorization mechanism, owner-client
identity, database-credential channel, or provider-credential behavior. It does
not make telemetry authoritative, promise cross-epoch token equality, or permit
recovery of a caller identifier from a token.
