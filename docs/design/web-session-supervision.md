# Web session supervision

This document describes committed work that is not built; it extends
[sessions-and-transcript](../spec/sessions-and-transcript.md) and is deleted
when the work lands.

The session timeline descriptor carries nullable supervision evidence from the
same read snapshot as its other facts. The evidence contains the closed failure
class, the retained sanitized cause code, and whether operator reconciliation is
pending. Storage records, application read facts, browser DTOs, and rendered
labels remain distinct representations.

The descriptor reads supervision independently of lifecycle reconstitution and
transcript detail decoding. A corrupt transcript remains unavailable while its
valid supervision evidence remains readable. Unknown failure classes fail
closed. The browser receives no unsanitized diagnostic payload or credential
material.

Pending supervision displays recovery required in the session header and message
composer status. The retained failure class and cause remain visible after
reconciliation, with the pending state distinguished. Active and queued turn
counts retain their meaning; pending supervision does not fabricate a terminal
turn, a completed park, or a new admission decision.

No endpoint, recovery command, retry policy, migration, or credential behavior
is added.

Acceptance requires a corrupt-transcript fixture whose descriptor still exposes
supervision, a browser scenario showing that evidence beside a failed transcript
read, and generated-contract rejection of unknown failure classes. Sessions
without supervision retain their existing presentation and command behavior.
