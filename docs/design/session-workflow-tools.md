# Session workflow tools

Sessions receive six ordinary daemon tools through `signalbox-tools-workflows`.
The invoking session and logical tool request come from trusted dispatch
correlation. Model arguments cannot select the caller or mutation identities.

| Tool                | Arguments                                                                                            | Grant                                         | Default posture | Shared program handler                                                     |
| ------------------- | ---------------------------------------------------------------------------------------------------- | --------------------------------------------- | --------------- | -------------------------------------------------------------------------- |
| `workflow_list`     | `after: UUID \| null`                                                                                | `list`                                        | automatic       | Retained run inventory; no socket list handler exists                      |
| `workflow_read`     | `run_id: UUID`                                                                                       | `read`                                        | automatic       | `handle_read_program`                                                      |
| `workflow_start`    | `name: string, revision: string, input: byte[]`                                                      | `start`, matching name                        | delegated       | Resolve registration, then `handle_start_program`                          |
| `workflow_stop`     | `run_id: UUID`                                                                                       | `stop`                                        | delegated       | `handle_cancel_program_run`                                                |
| `workflow_replay`   | `run_id: UUID`                                                                                       | `replay`, matching retained registration name | delegated       | Resolve retained registration and exact input, then `handle_start_program` |
| `workflow_register` | `name: string, revision: string, source_path: string, artifact_path: string, grants: ProgramGrant[]` | `register`, matching name                     | delegated       | `handle_register_program`, JavaScript only                                 |

List enumerates all retained registered runs in run-identity order, with
registration identity, name and revision, state, started and terminal times, and
an `own_run` marker for runs started or replayed by the caller. Each response
fits the tool-result bound and returns a nullable exclusive `next_after` cursor.
Read returns the socket's frame-bounded input and result prefixes and extents,
plus registration name, revision and journal length. Stop preserves the socket's
`applied`, `not_found` and `already_terminal` receipt algebra.

Replay creates a new run pinned to the original registration and retained exact
input bytes. It does not resume the original journal. Registration reads source
bytes and UTF-8 artifact text from paths confined to the calling session's
workspace; native registration is unavailable.

Each template's `workflow_tools` table grants operations explicitly. List, read
and stop take an `enabled` flag; start, replay and register take `names`, either
a list of exact registration names or `"*"`. Each operation accepts `posture`
with the existing `auto`, `delegated` and `human` values. Missing operations are
denied. The reloadable template catalog supplies policy by the session's
retained template identity; each proposed call freezes its selected posture.
Execution checks its operation and target grant independently of approval and
returns a retained typed tool failure on refusal. Approval cannot widen a grant.

Start, stop, replay and register derive durable mutation identities from the
logical tool request, independently of physical execution attempts. Equal retry
returns the recorded admission or cancellation receipt; conflicting reuse keeps
the socket error. Existing registrations, run bindings and cancellation receipts
remain authoritative. Caller attribution derives from the durable tool request.
The socket and tool adapters share the program command implementation lifted
from `process_runtime/program.rs`; neither adapter owns another cancellation
path or run repository.

The approval-judge corpus adds `workflow_tools` with live and offline cases for
an in-grant start, an out-of-grant start, stopping another session's run, and
registering workspace source. The existing judge and frozen-posture machinery
decide every operation.

Not built: scheduling, quotas, new event kinds, UI, session-native registration,
or changes to workflow execution and journal semantics.
