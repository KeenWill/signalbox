# Ownership seam

Compiled-in ownership modules contact daemon core through one crate boundary.
The boundary exposes eight lifecycle event families from a module-specific
outbox cursor: session creation, session state change, session terminal, turn
terminal, goal change, command settlement, injection settlement, and session
ownership change. Every other core outbox event advances the module cursor
without becoming module input.

The output boundary accepts the existing typed create-session, submit-input,
goal attach and resume, release-start, sticky stop, adopt, and ownership-release
commands. Core handles them under an authenticated module principal. Modules do
not mint turn, input, or frontier identities. Submission returns the exact
durable command result; settlement events remain replayable lifecycle input.

Each module owns reconstructible or module-local state in a `mod_` PostgreSQL
schema. The schema is owned by a non-login role with no direct privileges on
core tables. Module tables may use mutable projections or delete releasable
state; they do not reference core tables and can be rebuilt from core events and
the external source.

Module crates depend on `signalbox-ownership-seam`, not the domain, application,
or persistence crates. The ownership-seam checker rejects those dependency and
import edges, module SQL that names `public` relations, and core SQL that names
`mod_` relations. PostgreSQL grants independently deny direct core-table reads
and cross-schema references.
