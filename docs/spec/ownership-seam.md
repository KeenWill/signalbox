# Ownership seam

Compiled-in ownership modules contact daemon core through one crate boundary.
The boundary exposes eight lifecycle event families from a module-specific
outbox cursor: session creation, session state change, session terminal, turn
terminal, goal change, command settlement, injection settlement, and session
ownership change. Every other core outbox event advances the module cursor
without becoming module input.

The lifecycle source also exposes whether a session has a durable terminal fact,
independently of the module's cursor.

The output boundary exposes checked wrappers for the existing typed
create-session, submit-input, goal attach and resume, release-start, sticky
stop, adopt, and ownership-release commands. Modules do not mint turn, input, or
frontier identities. Repository watch is the first composed module consumer; its
daemon-owned command sink submits through core command repositories, and
settlement events remain replayable lifecycle input.

Each module owns reconstructible or module-local state in a `mod_` PostgreSQL
schema. The schema is owned by a dedicated login role with no membership path
back to the core identity and no direct privileges on core tables or functions.
Resetting the active role therefore retains the confined module identity. Module
tables may use mutable projections or delete releasable state; they do not
reference core tables and can be rebuilt from core events and the external
source.

Module crates depend on `signalbox-ownership-seam`, not the domain, application,
or persistence crates. The ownership-seam checker rejects those dependency and
import edges, module SQL that names `public` relations, and core SQL that names
`mod_` relations. PostgreSQL grants independently deny direct core-table reads,
core-function execution, and cross-schema references.

The reload-intent input carries the command identity, checked per-repository
rule sets, and rule-set digest. The module activates the set atomically and
idempotently by identity and digest, retaining each repository's event tail in
the activation transaction. Delivery grants neither role access to the other's
tables; [process protocol](process-protocol.md) owns intent persistence and
recovery.
