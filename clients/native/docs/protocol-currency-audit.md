# Native protocol-currency audit

This audit records the native client's drift from the current process protocol.
It is an implementation inventory, not a protocol specification. The normative
contracts remain the repository specifications and Rust protocol types linked
below.

Verified against repository head `849e8316` and a read-only live observation on
2026-08-01.

## Scope and method

The audit compared:

- the [process protocol](../../../docs/spec/process-protocol.md),
  [tool loop](../../../docs/spec/tool-loop.md), and
  [review workflow contract](../../../docs/spec/review-workflows.md);
- the current
  [`ClientRequest` and `ServerMessage` types](../../../crates/process-protocol/src/lib.rs)
  and their daemon/client consumers;
- the native [wire model](../Sources/SignalboxModels/ProcessProtocol.swift),
  [synchronizer](../Sources/SignalboxClient/SessionSynchronization.swift), and
  [timeline projector](../Sources/SignalboxClient/ProcessTranscriptProjector.swift);
- read-only `list`, transcript, and follow observations from existing live
  sessions. No live mutation was sent.

The live transcript sample exercised a completed turn, user and assistant text,
a terminal model-call usage record, and a completion marker. Follow confirmed
that the same durable conversation can be reconstructed through the streaming
read surface. Live observation is evidence of exercised shapes, not evidence
that unobserved protocol variants are absent.

The current Rust protocol has 41 client request verbs and 58 server message
kinds. Before this work, native modeled 16 request verbs and 35 server message
kinds, plus a generic unknown-message envelope. All 12 current durable
`SessionEvent` variants and all four current text-entry variants were named, but
some nested future variants were rejected during synchronization. Native named
eight of the nine current non-text transcript-entry variants.

Severity means user impact if the shape is encountered: **high** loses the read
surface or its synchronization, **medium** preserves access but loses or
misstates material information, and **low** omits secondary provenance.
Disposition counts are by the gap rows below, not by individual wire variants:
12 close-now, 10 staged, and 1 report-only.

## Close-now gaps

| ID  | Severity | Gap                                                                                                                                                                                    | Disposition                                                                                                                                  |
| --- | -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| C01 | High     | A server frame with a later numeric protocol version failed enum decoding before the client could classify the version mismatch.                                                       | **close-now** — retain an unknown numeric version and let the exchange report incompatibility explicitly.                                    |
| C02 | High     | Any added object member, including a member on a known frame, message, nested state, error, or detail, erased the known projection.                                                    | **close-now** — ignore added members while continuing to require and type-check known members.                                               |
| C03 | High     | Future turn, current-model-call, model-call-transition, and tool-batch-transition state variants forced snapshot or follow recovery.                                                   | **close-now** — retain typed unknown state payloads and advance valid snapshot/follow cursors with a diagnostic.                             |
| C04 | High     | Future non-text and text transcript-entry variants decoded generically but were rejected by the authoritative snapshot accumulator.                                                    | **close-now** — admit well-formed unknown entries and project a visibly unrecognized timeline card; malformed known entries remain failures. |
| C05 | Medium   | Future imported source-speaker wrapper kinds, attested speaker values, content kinds, and source-format values either erased an entry/page or failed it.                               | **close-now** — retain the wire token/payload and show an unrecognized label.                                                                |
| C06 | Medium   | Future failed-call dispositions and causes, terminal model-call dispositions, process error codes, and conversation cursor origins failed scalar enum decoding.                        | **close-now** — retain unknown scalar values and use conservative labels or explicit protocol errors.                                        |
| C07 | Medium   | A future durable `SessionEvent` was preserved and diagnosed by synchronization but produced no timeline row.                                                                           | **close-now** — add a stable, visibly unrecognized timeline entry without interpreting its payload.                                          |
| C08 | Medium   | The `model_identity_changed` transcript entry was the only current non-text entry kind native could not name.                                                                          | **close-now** — decode it and show its selected model and defaults version in the existing timeline idiom.                                   |
| C09 | Medium   | `transcript_model_call_usage` decoded and participated in snapshot counts but the projector silently discarded all four nullable token fields. This occurred in live transcript reads. | **close-now** — render a minimal typed usage entry for the owning model call.                                                                |
| C10 | Medium   | `context_summary` was rendered as an ordinary assistant message, hiding that the text is compacted context.                                                                            | **close-now** — retain the text while giving it a distinct typed timeline label.                                                             |
| C11 | Medium   | `plan_read` tool results were shown only as an undifferentiated raw JSON tool result.                                                                                                  | **close-now** — recognize the current tool name and render a minimal faithful plan read in the existing tool card.                           |
| C12 | Medium   | `plan_write` arguments and results were shown only as undifferentiated raw JSON.                                                                                                       | **close-now** — recognize the current tool name and render a minimal faithful plan update in the existing tool card.                         |

## Staged gaps

| ID  | Severity | Gap                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | Disposition                                                                                         |
| --- | -------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| S01 | Medium   | Native has no `create_session_from_template` or `list_templates` request and no `templates_start`, `template_summary`, or `templates_end` response model.                                                                                                                                                                                                                                                                                                                                                                                                                                                  | **staged** — design template information architecture before adding commands or screens.            |
| S02 | High     | Native's `submit_input` always sends `start_when_idle`; it cannot encode `steer` or `queue`, an explicit null `expected_defaults_version`, or decode `steering_submitted`.                                                                                                                                                                                                                                                                                                                                                                                                                                 | **staged** — design active-turn submission controls and delivery-state presentation.                |
| S03 | Medium   | Native has no `compact_session` request or `session_compacted` receipt, although it can read compaction events and context summaries.                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | **staged** — design the explicit compaction flow; C10 only fixes its read presentation.             |
| S04 | Medium   | Native reads session defaults but drops `system_prompt` in its application model and has no `replace_session_defaults` request or `session_defaults_replaced` receipt.                                                                                                                                                                                                                                                                                                                                                                                                                                     | **staged** — design a full defaults editor with epoch-conflict handling.                            |
| S05 | High     | Native has no `reconcile_turn` request despite decoding reconciliation-required turn and event states.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | **staged** — design the reconcile flow and recovery choices.                                        |
| S06 | Medium   | Native lacks the review mutation verbs `create_review_target`, `start_review_run`, `activate_review_pass`, `complete_review_pass`, `record_review_findings`, `record_review_finding_event`, `reserve_review_external_link`, `attach_review_external_link`, `start_review_orchestration`, `record_review_import_outcome`, `record_review_concern_outcome`, `record_review_judgment_plan`, `record_review_judgment_effect`, `record_review_repair_outcomes`, and `record_review_publication_outcomes`. It also lacks their creation, activation, completion, recording, linking, and orchestration receipts. | **staged** — design review workflow UI rather than expose raw commands.                             |
| S07 | Medium   | Native lacks `read_review_target`, `read_review_run`, `read_review_finding`, `list_review_findings`, and `read_review_orchestration`, plus `review_target`, `review_run`, `review_finding`, the three finding-page messages, and `review_orchestration`.                                                                                                                                                                                                                                                                                                                                                   | **staged** — design typed review read surfaces with the workflow UI.                                |
| S08 | Medium   | The daemon template catalog exposes only template name and bundle version, which is insufficient for a useful native template browser without another source of display metadata.                                                                                                                                                                                                                                                                                                                                                                                                                          | **staged** — daemon-side finding; define public template summary metadata before templates IA.      |
| S09 | Medium   | The current process protocol exposes no independent goal-mode read model for a native goal-mode display.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | **staged** — daemon-side finding; establish a read contract before designing goal-mode UI.          |
| S10 | Medium   | Plans are available only as model tool calls/results; there is no independent client plan-read verb or server projection.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  | **staged** — daemon-side finding; C11/C12 improve transcript fidelity without inventing a plan API. |

## Report-only gap

| ID  | Severity | Gap                                                                                                                                       | Disposition                                                                                  |
| --- | -------- | ----------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| R01 | Low      | Session metadata `last_writer` decodes but is not carried into native presentation, so replacement provenance is unavailable to the user. | **report-only** — retain as a presentation backlog item; no existing timeline idiom fits it. |

## Current shape catalog

The 25 request verbs absent from native are the five non-review verbs in S01
through S05 and the 20 review verbs in S06 and S07. The 23 named server kinds
absent before this work are `steering_submitted`; the three template sequence
kinds; `session_defaults_replaced`; `session_compacted`; and these 17 review
kinds: `review_target_created`, `review_run_started`, `review_pass_activated`,
`review_pass_completed`, `review_findings_recorded`,
`review_finding_event_recorded`, `review_external_link_reserved`,
`review_external_link_attached`, `review_target`, `review_run`,
`review_finding`, `review_findings_start`, `review_finding_item`,
`review_findings_end`, `review_orchestration_started`,
`review_orchestration_advanced`, and `review_orchestration`.

The durable event family itself is current: `session_created`, `input_accepted`,
`turn_activated`, `model_call_transition`, `tool_batch_transition`,
`context_compacted`, `turn_completed`, `turn_failed`, `turn_refused`,
`turn_cancelled`, `turn_reconciliation_required`, and
`turn_tool_reconciliation_required` are all decoded. Native also carries a
generic unknown-event representation for forward compatibility. The gap was
acceptance or presentation of newer nested/event content, covered by C03 and
C07.

The non-text transcript family is `model_identity_changed`,
`assistant_tool_use`, `tool_execution_result`, `tool_denied`, `tool_closed`,
`turn_completed`, `turn_failed`, `turn_cancelled`, and `imported`, plus the
generic unknown-entry representation. The text family is `user`, `assistant`,
`context_summary`, and `imported`, plus its generic unknown-entry
representation. C04, C08, and C10 cover their currency gaps.
