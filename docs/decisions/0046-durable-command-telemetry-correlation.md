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

[ADR-0044](0044-hub-runtime-foundations.md) prohibits raw caller-supplied
`DurableCommandId` values in telemetry and asked its observability boundary for
a stable, non-reversible correlation token. That requirement is incomplete.
[ADR-0033](0033-identity-generation-supply-and-encoding.md) accepts every
non-sentinel UUID from a caller, including predictable values. An unkeyed digest
can therefore be reversed by enumerating likely UUIDs. A process-random key
prevents enumeration but loses correlation after restart.

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
contains only lowercase ASCII letters, digits, and hyphens, is at most 32 bytes,
and is never reused for different key bytes. `digest` is unpadded base64url of
the first 16 bytes of:

```text
HMAC-SHA-256(key, "signalbox/durable-command-telemetry/v1\0" || uuid-bytes)
```

`uuid-bytes` is the 16-byte network-order UUID representation fixed by
[ADR-0033](0033-identity-generation-supply-and-encoding.md). The literal domain
separator, NUL byte, HMAC algorithm, truncation, and encoding are part of
version `dc1`; changing any of them requires a new token version.
Implementations use a reviewed HMAC-SHA-256 implementation, not a
repository-owned cryptographic primitive. This record adds no dependency by
itself; a future implementation pull request may select a focused cryptographic
crate under the repository dependency rules.

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

The deployment supplies the active key and its `key-id` to hubd as a read-only,
volume-mounted secret file pair. They never arrive in command-line arguments,
ordinary configuration files, process environment variables, Postgres, domain
values, protocol messages, or logs. Only hubd's correlation-token component
reads the files. Library code receives an opaque token-derivation capability,
never key bytes. The upstream secret manager and Kubernetes manifest shape
remain deployment mechanics, preserving
[ADR-0017](0017-credential-lifecycle.md)'s ownership of its credential classes
and [ADR-0032](0032-postgres-implementation-dependencies.md)'s open
database-credential delivery question.

Hubd reads and validates exactly one epoch during startup and keeps it for that
process lifetime. Updating mounted files does not change a running process's
epoch. Rotation activates only through a deliberate hub restart or rollout, so
one process never emits two tokens for the same command because files changed
between reads.

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

Raw caller-supplied `DurableCommandId` values and their UUID text, bytes,
integer form, prefixes, suffixes, or unkeyed digests never appear in telemetry,
panic text, or error formatting. Structured fields use the complete token and a
name that identifies it as correlation rather than command identity. Free-form
messages do not interpolate either the raw identifier or key material.

Registry-level and pre-claim events required by
[ADR-0044](0044-hub-runtime-foundations.md) use the token without inventing a
session key. Session-scoped events may include ADR-0044's permitted hub-minted
aggregate identifiers alongside it. Token derivation does not relax
[ADR-0017](0017-credential-lifecycle.md)'s credential redaction or ADR-0044's
user-content and payload redaction.

Missing, unreadable, malformed, non-32-byte, or inconsistent key files fail hubd
startup before migrations, recovery, scheduling, protocol admission, or worker
startup. An invalid `key-id` is likewise a configuration failure. There is no
fallback to a raw identifier, unkeyed digest, process-random key, or fixed
default key. If the already-validated in-memory derivation capability becomes
unavailable at runtime, the event may retain its fixed taxonomy and safe
hub-minted keys but omits command correlation; it must not substitute raw or
reversible material. The failure is itself reported without the command ID or
key.

## Invariants

- INV-002: token and key remain runtime/observability representations and never
  become domain, storage, or wire representations.
- INV-012: the token neither replaces nor creates owner-global durable-command
  identity or replay authority.
- INV-035: this separate observability secret is inaccessible to clients and
  runners, and no key material appears in logs or protocol values.

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

- **Pre-claim conflicting reuse:** registry lookup classifies the request under
  [ADR-0044](0044-hub-runtime-foundations.md) and logs `dc1.<key-id>.<digest>`
  without a session identity or raw caller UUID. Another replica in the same
  epoch emits the identical token.
- **Restart recovery:** the deployment supplies the same epoch after an ordinary
  restart, so recovery and earlier live-operation events correlate.
- **Routine rotation:** a rollout activates a fresh epoch. New events use the
  new `key-id`; an authorized investigator with a known command ID may derive
  both epoch tokens offline while the running hub knows only the current key.
- **Invalid deployment:** hubd cannot validate the mounted key pair and exits
  before migrations or work admission. It never downgrades to raw logging.

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
