# Living specification

The pages under `docs/spec/` are the specification of the behavior Signalbox
has: the contracts between crates and across the wire that an implementing agent
honors. Each page opens with a map for a reader who knows Signalbox as a whole
but not that subsystem, then states the decisions and contracts the code keeps.
`AGENTS.md` is the guidance for agents working on the repository; this file owns
the conventions the pages follow.

## Homes

A normative claim about a subsystem lives in exactly one of three places;
[architecture.md](../architecture.md) and [target-model.md](../target-model.md)
are orientation documents outside that rule. `docs/spec/` states built behavior
only. `docs/design/` holds one document per subsystem with committed but unbuilt
design, written for the agent that will build it; landed material is removed as
it lands, and the document is deleted when no planned capability remains.
[open-questions.md](../open-questions.md) holds undecided items. Nothing on a
spec page describes behavior the code lacks except the lines under Planned. Two
normative surfaces sit outside those homes:
[domain-spine.md](../domain-spine.md) mirrors the public API shapes of the
domain and application crates.

A design document is titled `<Subsystem> design`, opens with a preamble saying
it is not built and naming the spec page it extends, and has the sections Goal,
Design, Compatibility constraints, and Acceptance criteria. It keeps decisions,
shapes, transitions, and acceptance criteria, and links the spec page for built
behavior instead of restating it, except that a compatibility constraint states
the current behavior the design preserves. A foundation-weight change proposes
its semantics in that document at the bottom of the implementing stack; the spec
page changes with the code that builds them.

## Page shape

Every subsystem specification page, but not this conventions page, has one
title, one sentence saying what the subsystem is for, and exactly four sections
in this order: Overview, Design decisions, Boundary contracts, Planned.

Overview says what the subsystem is, its boundary, the shape of its data, its
major parts, and how they relate. It may name the core type, table, or function
a reader will look for, and a bounded inventory when that inventory is the
subsystem's data shape; it never otherwise enumerates types, fields, variants,
columns, or CLI flags, which live in the code, the migration, and the example
TOML. It is paragraphs, not lists, unless the content is a real sequence.

Design decisions states each rule in one sentence, with its reason in the same
sentence or in one "Why:" sentence after it. The failure a rule prevents is
stated only when a reader could not infer it. Owner rulings are decisions, and
so are fences, phrased as what is deliberately not done. No decision restates a
contract or a not-built line.

Boundary contracts holds the rules an implementing agent must honor, one
paragraph each, and names the enforcer when one exists. Each repo-wide contract
has one home page; every other page links to that page by name and never
restates the contract.

Planned has one line per committed unbuilt capability, naming it and linking its
design document, and reads `None.` when the subsystem has no committed unbuilt
capability. Nothing else about unbuilt design appears on the page.

## Prose standard

Sentences are plain and declarative, about twenty words, one idea each. Pages
carry no rationale narrative, no metaphor, no hedges, no editorial or
decision-source provenance, no history, and no pull-request or branch names.
Provenance a subsystem records is behavior and stays on the page. A version
number stays when it defines a wire or storage contract, and goes when it only
records when behavior changed. A page says what the system does, not what a
reader should do. Code identifiers appear only where the map names a core
mechanism, a decision names the thing it decides, or a contract names its
enforcer. A contract also names an identifier that is itself contract data, such
as a field name, a discriminator, or a preimage. A contract names its enforcer
by source path, crate, type, or function. Pages have no Open edges section and
no paragraph labelled as committed but unimplemented. Every link targets a page,
never an anchor, unless the anchor is a heading on the linking page.

## Pages

- [Sessions and the transcript](sessions-and-transcript.md)
- [Session lifecycle](session-lifecycle.md)
- [Turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md)
- [Goal mode](goal-mode.md)
- [Model-call execution](model-call-execution.md)
- [Tool loop](tool-loop.md)
- [Model-runtime substrate](runtime-substrate.md)
- [Model and session settings](model-session-settings.md)
- [Configuration and credentials](configuration-and-credentials.md)
- [Credential availability](credential-availability.md)
- [Identity, commands, and telemetry correlation](identity-and-commands.md)
- [Process protocol](process-protocol.md)
- [Persistence protocol](persistence-protocol.md)
- [Blob storage](blob-storage.md)
- [File and media interpretation](file-and-media.md)
- [Conversation import](conversation-import.md)
- [Runner protocol and placement](runner-protocol.md)
- [Repository watch and event dispatch](repo-watch.md)
- [Review workflows](review-workflows.md)
- [Git authority threat model](git-authority-threat-model.md)
- [Web egress threat model](web-egress-threat-model.md)
- [Workspace instructions and skills](workspace-instructions.md)
- [Program substrate](program-substrate.md)
- [Evaluation system](eval-system.md)

## Design documents

- [Sessions and the transcript design](../design/sessions-and-transcript.md)
- [Session lifecycle design](../design/session-lifecycle.md)
- [Turn lifecycle and scheduling design](../design/turn-lifecycle-and-scheduling.md)
- [Model-call execution design](../design/model-call-execution.md)
- [Tool loop design](../design/tool-loop.md)
- [Model-runtime substrate design](../design/runtime-substrate.md)
- [Model and session settings design](../design/model-session-settings.md)
- [Configuration and credentials design](../design/configuration-and-credentials.md)
- [Credential availability design](../design/credential-availability.md)
- [Identity and commands design](../design/identity-and-commands.md)
- [Process protocol design](../design/process-protocol.md)
- [Persistence protocol design](../design/persistence-protocol.md)
- [Blob storage design](../design/blob-storage.md)
- [File and media interpretation design](../design/file-and-media.md)
- [Conversation import design](../design/conversation-import.md)
- [Runner protocol design](../design/runner-protocol.md)
- [Repository watch design](../design/repo-watch.md)
- [Review workflows design](../design/review-workflows.md)
- [Git authority threat model design](../design/git-authority-threat-model.md)
- [Workspace instructions design](../design/workspace-instructions.md)
- [Program substrate design](../design/program-substrate.md)
- [Evaluation system design](../design/eval-system.md)
