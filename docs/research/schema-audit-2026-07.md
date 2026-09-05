# Signalbox schema audit refresh

> Dated research intake (2026-07-25), non-normative. Historical decisions live
> in git history, and current requirements live in the
> [living specification](../spec/README.md); those sources supersede anything
> stated here.

- Date: 2026-07-25
- Status: research intake and checkpoint recommendation
- Audited snapshot: `origin/main` @ `e62f4ee` — all 27 migration files,
  `202607180001` through `202607280001`
- Separately inspected prerequisite: `origin/agent/review-workflow-persistence`
  @ `5113c2a6`
- Scope: the complete composed PostgreSQL catalog, every post-2026-07-24
  migration, migration-wake residue, squash risk, and a baseline-reset plan

## Relationship to the 2026-07-24 audit

This document supersedes the current verdict of the
[2026-07-24 schema audit](schema-audit-2026-07-24.md) and extends its migration
history through `202607280001`. The earlier dated intake remains unchanged as
the record of its 18-file snapshot. Its nullable-column finding was fixed; its
general conclusion that the final schema is tight remains current; its estimate
of the history tax and its squash recommendation are replaced by the
catalog-backed findings below.

## Verdict

**Squash-worthy now; execute a full reset at the next owner checkpoint, not a
partial reset and not before the in-flight review-workflow schema lands or is
abandoned.**

The final 27-file catalog is healthy in behavior but no longer clean in
declaration. It contains two exact duplicate structural constraints:

- `submit_input_command_accepted_result_key` and
  `submit_input_command_general_applied_key` are the same three-column UNIQUE
  constraint and create duplicate unique indexes.
- `accepted_input_command_result_fk` and
  `accepted_input_general_command_result_fk` are the same three-column deferred
  foreign key.

It also carries systematic rename residue: `context_frontier_member` became the
physical `context_frontier_delta` table, but all five table constraints and four
table triggers retain `context_frontier_member_*` names. The resolved
`context_frontier_member` view itself is live behavior, not residue; the
[persistence protocol](../spec/persistence-protocol.md) and production queries
use it.

The chain is 16,888 lines. A schema-only dump of the composed catalog is 10,902
lines, a 5,986-line or 35% scale difference. That comparison is not an estimate
of mechanically deletable lines—the dump has its own boilerplate—but it confirms
the earlier audit's conclusion that reshape choreography and superseded
definitions are a material fraction of the set. The nine new files alone add
5,424 lines. A partial squash would retain the interleaved validator ladder,
duplicate constraints, prefix-conversion history, and history-pinning tests
while paying nearly all reset risk.

The historical owner checkpoint in
[commit f6db5e71](https://github.com/KeenWill/signalbox/commit/f6db5e71c)
permits a clean baseline while every database is disposable. The
[backlog entry](../agents/backlog.md#migration-baseline-reset-blocked-on-schema-audit-verdict-owner-checkpoint-call-size-s-m)
adds the remaining owner checkpoint gate.

## Audit method and composed inventory

I read every migration and applied all 27 in lexical order to an ephemeral
PostgreSQL 18.4 instance, the version pinned by the persistence integration
tests. Catalog inspection found:

- 48 tables, one view, 388 table columns, and 88 public functions;
- 509 constraints excluding PostgreSQL 18's catalogued NOT NULL constraints;
- 149 indexes: 132 backing primary/unique constraints and 17 independently
  declared indexes;
- 128 non-internal triggers, including row, constraint, and TRUNCATE triggers.

The exact-duplicate scan grouped constraints by table, type, and normalized
`pg_get_constraintdef`; it found only the UNIQUE pair and FK pair named in the
verdict. The equivalent index scan found only the corresponding duplicate UNIQUE
indexes.

In the table ledger below, `C/F/P/U/CT` means CHECK, foreign key, primary key,
UNIQUE, and constraint-trigger counts. Counts exclude NOT NULL constraints.
“Index” lists every independently declared index; primary and UNIQUE indexes are
represented by `P/U`. “Triggers” is the complete non-internal trigger count,
including TRUNCATE triggers. A migration listed for a table either created it or
changed its columns, constraints, indexes, triggers, or table-specific
validator.

### Table, constraint, index, and trigger ledger

| Table                                           | Shaped by migration suffixes                                                   | C/F/P/U/CT | Independent indexes                                                                                               | Triggers | Baseline assessment                                                                      |
| ----------------------------------------------- | ------------------------------------------------------------------------------ | ---------: | ----------------------------------------------------------------------------------------------------------------- | -------: | ---------------------------------------------------------------------------------------- |
| `accepted_input`                                | `180003`, `180004`, `180005`, `200001`, `220001`, `220004`, `220005`           |  8/6/1/8/2 | `accepted_input_consumed_by_model_call`; `accepted_input_pending_by_source_turn`                                  |        3 | Drop one of the two identical command-result FKs; otherwise final declaration is correct |
| `context_frontier`                              | `180004`, `260300`                                                             |  1/2/1/1/1 | —                                                                                                                 |        2 | Declare prefix column and FK directly                                                    |
| `context_frontier_delta`                        | `180004` as `context_frontier_member`; `240002`, `260300`                      |  1/2/1/1/1 | —                                                                                                                 |        4 | Declare directly and use `context_frontier_delta_*` physical names                       |
| `create_session_command`                        | `180001`, `240002`, `240006`                                                   |  9/3/1/1/0 | —                                                                                                                 |        2 | Final shape is correct                                                                   |
| `create_session_from_imported_frontier_command` | `240002`, `240006`                                                             | 11/4/1/1/0 | —                                                                                                                 |        2 | Final shape is correct                                                                   |
| `decide_tool_request_command`                   | `250001`                                                                       |  7/1/1/0/1 | —                                                                                                                 |        2 | Final shape is correct                                                                   |
| `durable_command`                               | `180001`, `180002`, `180003`, `240002`, `240006`, `250001`, `260101`           |  2/0/1/1/1 | —                                                                                                                 |        2 | Inline the final six-kind/version matrix                                                 |
| `hub_fence_state`                               | `230001`                                                                       |  2/0/1/0/0 | —                                                                                                                 |        2 | Keep as the first baseline phase                                                         |
| `imported_conversation`                         | `240001`, `240004`, `240005`, `270001`                                         |  7/0/1/1/1 | `imported_conversation_source_session_id_idx`                                                                     |        3 | Inline final format/version checks and optional lineage                                  |
| `imported_conversation_raw_record`              | `240001`                                                                       |  5/2/1/0/0 | `imported_conversation_raw_record_content_hash_idx`                                                               |        3 | Final shape is correct                                                                   |
| `imported_raw_source_record`                    | `240001`                                                                       |  2/0/1/0/1 | —                                                                                                                 |        3 | Final shape is correct                                                                   |
| `imported_session_seed`                         | `240002`                                                                       |  0/1/1/1/1 | —                                                                                                                 |        4 | Final shape is correct                                                                   |
| `imported_transcript_entry`                     | `240001`, `240002`                                                             |  6/2/1/4/0 | —                                                                                                                 |        3 | Final shape is correct                                                                   |
| `input_accepted_outbox_event`                   | `230001`                                                                       |  2/2/1/2/0 | —                                                                                                                 |        2 | Final shape is correct                                                                   |
| `model_call`                                    | `220001`, `220002`, `220003`, `220004`, `220005`, `240007`, `250001`           |  6/3/1/3/2 | `model_call_by_turn_attempt`                                                                                      |        3 | Declare total, non-empty `credential_reference` directly                                 |
| `model_call_transition_outbox_event`            | `220001`, `220005`                                                             |  4/2/1/1/0 | —                                                                                                                 |        2 | Inline the final stopped-state arm                                                       |
| `outbox_delivery_state`                         | `200002`                                                                       |  3/0/1/0/0 | —                                                                                                                 |        3 | Final shape is correct                                                                   |
| `outbox_event`                                  | `200002`, `210001`, `220001`, `220005`, `230001`, `250001`                     |  3/1/1/1/1 | `outbox_event_by_session_sequence`                                                                                |        4 | Inline the final event-kind/version matrix                                               |
| `outbox_sequence_state`                         | `200002`                                                                       |  3/0/1/0/1 | —                                                                                                                 |        3 | Final shape is correct                                                                   |
| `queued_input_origin`                           | `180003`, `180004`, `180005`, `220001`, `220005`, `250001`                     | 11/5/1/4/0 | `queued_input_origin_by_session_position`                                                                         |        2 | Inline final provenance shape; retain the v1 defaulting trigger while v1 is supported    |
| `replace_session_defaults_command`              | `180002`, `240006`                                                             | 13/2/1/0/0 | —                                                                                                                 |        1 | Final shape is correct                                                                   |
| `replace_session_metadata_command`              | `260101`                                                                       | 12/3/1/1/0 | —                                                                                                                 |        3 | Final shape is correct                                                                   |
| `replace_session_metadata_command_attribute`    | `260101`                                                                       |  2/1/1/0/0 | —                                                                                                                 |        3 | Final shape is correct                                                                   |
| `replace_session_metadata_command_tag`          | `260101`                                                                       |  2/1/1/0/0 | —                                                                                                                 |        3 | Final shape is correct                                                                   |
| `semantic_transcript_entry`                     | `180004`, `220001`, `220004`, `220005`, `240002`, `250001`                     | 4/11/1/9/3 | —                                                                                                                 |        5 | Inline the final exhaustive payload shape and validators                                 |
| `session`                                       | `180001`, `180004`, `240002`                                                   |  5/3/1/2/2 | —                                                                                                                 |        3 | Inline imported ancestry and final creation guards                                       |
| `session_created_outbox_event`                  | `200002`                                                                       |  2/1/1/1/0 | —                                                                                                                 |        2 | Final shape is correct                                                                   |
| `session_current_defaults`                      | `180001`                                                                       |  1/1/1/0/0 | —                                                                                                                 |        0 | Final shape is correct                                                                   |
| `session_defaults_version`                      | `180001`, `240006`                                                             |  4/1/1/1/0 | —                                                                                                                 |        1 | Retain the v1-compatible `disabled` default                                              |
| `session_metadata`                              | `260101`                                                                       |  4/3/1/0/1 | —                                                                                                                 |        5 | Final shape is correct                                                                   |
| `session_metadata_attribute`                    | `260101`                                                                       |  2/1/1/0/1 | —                                                                                                                 |        3 | Final shape is correct                                                                   |
| `session_metadata_installation`                 | `260101`                                                                       |  0/1/1/0/0 | —                                                                                                                 |        4 | Final shape is correct                                                                   |
| `session_metadata_tag`                          | `260101`                                                                       |  2/1/1/0/1 | `session_metadata_tag_lookup`                                                                                     |        3 | Final shape is correct                                                                   |
| `session_scheduler`                             | `180004`                                                                       |  0/1/1/0/0 | —                                                                                                                 |        1 | Declare directly; no backfill                                                            |
| `submit_input_command`                          | `180003`, `180005`, `200001`, `220001`, `220005`, `280001`                     | 19/9/1/4/2 | —                                                                                                                 |        3 | Keep one accepted-result UNIQUE; inline the final parked-approval arm                    |
| `tool_approval_decision`                        | `250001`                                                                       |  4/2/1/1/4 | —                                                                                                                 |        5 | Final shape is correct                                                                   |
| `tool_attempt`                                  | `250001`                                                                       |  8/2/1/4/2 | `tool_attempt_one_live_per_turn`                                                                                  |        3 | Final shape is correct                                                                   |
| `tool_batch_transition_outbox_event`            | `250001`                                                                       |  4/4/1/0/0 | `tool_batch_transition_outbox_frontier_once`; `tool_batch_transition_outbox_recovery_once`                        |        2 | Final shape is correct                                                                   |
| `tool_request`                                  | `250001`                                                                       |  5/1/1/4/1 | —                                                                                                                 |        2 | Final shape is correct                                                                   |
| `tool_round`                                    | `250001`                                                                       |  2/2/1/1/1 | —                                                                                                                 |        2 | Final shape is correct                                                                   |
| `turn_activated_outbox_event`                   | `230001`                                                                       |  2/2/1/2/0 | —                                                                                                                 |        2 | Final shape is correct                                                                   |
| `turn_attempt`                                  | `180004`, `220003`, `220005`, `250001`                                         |  4/4/1/3/3 | `turn_attempt_by_turn_session`; `turn_attempt_one_initial_per_turn`; `turn_attempt_one_live_per_turn`             |        4 | Inline the final attempt/tool/interrupt shape                                            |
| `turn_cancelled_outbox_event`                   | `220005`                                                                       |  2/4/1/3/0 | —                                                                                                                 |        2 | Final shape is correct                                                                   |
| `turn_completed_outbox_event`                   | `220001`                                                                       |  2/5/1/4/0 | —                                                                                                                 |        2 | Final shape is correct                                                                   |
| `turn_failed_outbox_event`                      | `210001`                                                                       |  2/4/1/3/0 | —                                                                                                                 |        2 | Final shape is correct                                                                   |
| `turn_lifecycle`                                | `180004`, `200006`, `220001`, `220003`, `220004`, `220005`, `240003`, `250001` | 7/13/1/4/3 | `turn_lifecycle_by_session_position`; `turn_lifecycle_one_active_per_session`; `turn_lifecycle_queued_by_session` |        4 | Inline the final lifecycle/tool-loop validators; review workflow will reshape it again   |
| `turn_reconciliation_required_outbox_event`     | `220005`, `250001`                                                             |  3/5/1/4/0 | —                                                                                                                 |        2 | Inline the final model-call/tool-attempt XOR                                             |
| `turn_refused_outbox_event`                     | `220001`                                                                       |  2/4/1/3/0 | —                                                                                                                 |        2 | Final shape is correct                                                                   |

The one view is `context_frontier_member`, created by `260300` as the resolved
projection over `context_frontier` plus `context_frontier_delta`. It remains in
the clean baseline.

The complete independent-index inventory is the 17 names in the table. No
independent index is redundant. The only duplicate index definitions are the two
constraint-backed `submit_input_command` UNIQUE indexes.

### Complete trigger-name inventory

This ledger names all 128 final triggers once. The table ledger above supplies
their shaping migrations.

- `accepted_input`: `accepted_input_is_append_only`,
  `accepted_input_pending_requires_active_source`,
  `accepted_input_requires_steering_final_state`
- `context_frontier`: `context_frontier_is_append_only`,
  `context_frontier_requires_complete_membership`
- `context_frontier_delta`: `context_frontier_member_cannot_be_truncated`,
  `context_frontier_member_is_append_only`,
  `context_frontier_member_rechecks_declared_count`,
  `context_frontier_member_stays_within_declared_count`
- `create_session_command`: `create_session_command_cannot_be_truncated`,
  `create_session_command_is_append_only`
- `create_session_from_imported_frontier_command`:
  `create_session_from_imported_frontier_command_is_append_only`,
  `imported_frontier_command_cannot_be_truncated`
- `decide_tool_request_command`: `decide_tool_request_command_is_append_only`,
  `decide_tool_request_command_requires_effect`
- `durable_command`: `durable_command_is_append_only`,
  `durable_command_requires_typed_record`
- `hub_fence_state`: `hub_fence_state_cannot_be_truncated`,
  `hub_fence_state_change_is_guarded`
- `imported_conversation`: `imported_conversation_cannot_be_truncated`,
  `imported_conversation_is_append_only`,
  `imported_conversation_requires_complete_membership`
- `imported_conversation_raw_record`:
  `imported_conversation_raw_record_cannot_be_truncated`,
  `imported_conversation_raw_record_is_append_only`,
  `imported_raw_record_stays_within_declared_count`
- `imported_raw_source_record`:
  `imported_raw_source_record_cannot_be_truncated`,
  `imported_raw_source_record_is_append_only`,
  `imported_raw_source_record_requires_occurrence`
- `imported_session_seed`: `imported_seed_requires_imported_ancestry`,
  `imported_session_seed_cannot_be_truncated`,
  `imported_session_seed_is_append_only`,
  `imported_session_seed_records_creation_transaction`
- `imported_transcript_entry`: `imported_entry_stays_within_declared_counts`,
  `imported_transcript_entry_cannot_be_truncated`,
  `imported_transcript_entry_is_append_only`
- `input_accepted_outbox_event`:
  `input_accepted_outbox_event_cannot_be_truncated`,
  `input_accepted_outbox_event_is_append_only`
- `model_call`: `model_call_changes_are_guarded`,
  `model_call_requires_complete_final_state`,
  `model_call_requires_failed_terminal_execution`
- `model_call_transition_outbox_event`:
  `model_call_transition_outbox_event_cannot_be_truncated`,
  `model_call_transition_outbox_event_is_append_only`
- `outbox_delivery_state`: `outbox_delivery_advances_prefix`,
  `outbox_delivery_state_cannot_be_deleted`,
  `outbox_delivery_state_cannot_be_truncated`
- `outbox_event`: `outbox_event_allocates_sequence`,
  `outbox_event_cannot_be_truncated`, `outbox_event_is_append_only`,
  `outbox_event_requires_typed_record`
- `outbox_sequence_state`: `outbox_sequence_requires_event`,
  `outbox_sequence_state_cannot_be_deleted`,
  `outbox_sequence_state_cannot_be_truncated`
- `queued_input_origin`: `queued_input_origin_defaults_v1_tool_auto_approval`,
  `queued_input_origin_is_append_only`
- `replace_session_defaults_command`:
  `replace_session_defaults_command_is_append_only`
- `replace_session_metadata_command`:
  `replace_session_metadata_command_is_append_only`,
  `replace_session_metadata_command_records_installation`,
  `replace_session_metadata_command_truncate_is_rejected`
- `replace_session_metadata_command_attribute`:
  `replace_session_metadata_command_attribute_insert_before_seal`,
  `replace_session_metadata_command_attribute_is_append_only`,
  `replace_session_metadata_command_attribute_truncate_is_rejected`
- `replace_session_metadata_command_tag`:
  `replace_session_metadata_command_tag_insert_before_seal`,
  `replace_session_metadata_command_tag_is_append_only`,
  `replace_session_metadata_command_tag_truncate_is_rejected`
- `semantic_transcript_entry`: `imported_semantic_entry_seed_is_sealed`,
  `semantic_entry_one_logical_tool_result`,
  `semantic_entry_requires_matching_turn_state`,
  `semantic_entry_requires_steering_final_state`,
  `semantic_transcript_entry_is_append_only`
- `session`: `session_is_append_only`, `session_requires_creation_command`,
  `session_requires_imported_seed`
- `session_created_outbox_event`:
  `session_created_outbox_event_cannot_be_truncated`,
  `session_created_outbox_event_is_append_only`
- `session_defaults_version`: `session_defaults_version_is_append_only`
- `session_metadata`: `session_metadata_delete_is_rejected`,
  `session_metadata_identity_is_immutable`, `session_metadata_matches_receipt`,
  `session_metadata_receipt_reinstallation_is_rejected`,
  `session_metadata_truncate_is_rejected`
- `session_metadata_attribute`: `session_metadata_attribute_matches_receipt`,
  `session_metadata_attribute_truncate_is_rejected`,
  `session_metadata_attribute_update_is_rejected`
- `session_metadata_installation`:
  `session_metadata_installation_is_append_only`,
  `session_metadata_installation_matches_receipt`,
  `session_metadata_installation_requires_current`,
  `session_metadata_installation_truncate_is_rejected`
- `session_metadata_tag`: `session_metadata_tag_matches_receipt`,
  `session_metadata_tag_truncate_is_rejected`,
  `session_metadata_tag_update_is_rejected`
- `session_scheduler`: `session_scheduler_is_append_only`
- `submit_input_command`: `submit_input_command_is_append_only`,
  `submit_input_command_requires_correlated_effect`,
  `submit_input_command_requires_interrupt_effect`
- `tool_approval_decision`: `denied_tool_request_has_no_attempt`,
  `owner_tool_approval_requires_command`,
  `tool_approval_decision_is_append_only`,
  `tool_approval_requires_complete_final_state`,
  `tool_approval_session_blanket_provenance`
- `tool_attempt`: `tool_attempt_changes_are_guarded`,
  `tool_attempt_requires_approval`, `tool_attempt_requires_complete_final_state`
- `tool_batch_transition_outbox_event`:
  `tool_batch_transition_outbox_event_cannot_be_truncated`,
  `tool_batch_transition_outbox_event_is_append_only`
- `tool_request`: `tool_request_is_append_only`,
  `tool_request_requires_complete_final_state`
- `tool_round`: `tool_round_is_append_only`,
  `tool_round_requires_complete_final_state`
- `turn_activated_outbox_event`:
  `turn_activated_outbox_event_cannot_be_truncated`,
  `turn_activated_outbox_event_is_append_only`
- `turn_attempt`: `turn_attempt_changes_are_guarded`,
  `turn_attempt_requires_complete_final_state`,
  `turn_attempt_requires_failed_terminal_execution`,
  `turn_attempt_requires_interrupt_proof`
- `turn_cancelled_outbox_event`:
  `turn_cancelled_outbox_event_cannot_be_truncated`,
  `turn_cancelled_outbox_event_is_append_only`
- `turn_completed_outbox_event`:
  `turn_completed_outbox_event_cannot_be_truncated`,
  `turn_completed_outbox_event_is_append_only`
- `turn_failed_outbox_event`: `turn_failed_outbox_event_cannot_be_truncated`,
  `turn_failed_outbox_event_is_append_only`
- `turn_lifecycle`: `turn_lifecycle_changes_are_guarded`,
  `turn_lifecycle_requires_complete_final_state`,
  `turn_lifecycle_requires_failed_terminal_execution`,
  `turn_terminal_requires_closed_pending_steering`
- `turn_reconciliation_required_outbox_event`:
  `turn_reconciliation_required_outbox_event_cannot_be_truncated`,
  `turn_reconciliation_required_outbox_event_is_append_only`
- `turn_refused_outbox_event`: `turn_refused_outbox_event_cannot_be_truncated`,
  `turn_refused_outbox_event_is_append_only`

`session_current_defaults` has no user trigger. Its current pointer is instead
bound by foreign key and command transaction behavior.

## Reverification of the prior audit

- The prior diseased column is fixed. Migration `240007` makes
  `model_call.credential_reference` NOT NULL and non-empty, and the store no
  longer carries the NULL-read branch. This matches the authorizing change in
  [PR #217](https://github.com/KeenWill/signalbox/pull/217).
- The new nullable fields are domain optionality, not compatibility: imported
  source-session lineage is unknown or conflicting evidence; context-frontier
  prefix identity is optional; tool, metadata, and lifecycle payload columns are
  closed by shape checks and deferred validators. The tool-default column on
  `queued_input_origin` is nullable only in the source-configuration-reference
  arm.
- The four `dangerous_tool_auto_approval DEFAULT 'disabled'` declarations and
  `queued_input_origin_defaults_v1_tool_auto_approval` remain deliberate while
  storage version 1 is accepted. They implement the v1 decoding rule; they are
  not phantom-row backfills.
- The live `require_submit_input_legacy_effect_correlation` name remains the
  mild naming residue identified in the earlier audit. The function is the
  non-interrupt validator and should be declared in the baseline as
  `require_non_interrupt_submit_input_effect_correlation`, with its one final
  trigger reference updated.

## Full audit of the nine post-2026-07-24 migrations

| Migration                                             | What it does                                                                                                                                                              | Correct-choice comparison                                                                                                                                                    | Baseline treatment                                                                                                          |
| ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| `240004_conversation_import_converter_v2`             | Replaces the converter-version CHECK to admit versions 1 and 2                                                                                                            | Final check is correct; the file is only a superseded CHECK arm                                                                                                              | Fold the final check into `imported_conversation`                                                                           |
| `240005_conversation_import_codex_format`             | Replaces the source-format CHECK and adds the format/version relation                                                                                                     | The separate closed sets plus relational CHECK are intentional fail-closed diagnostics, not redundant history                                                                | Fold all final checks into `imported_conversation`                                                                          |
| `240006_tool_defaults_approval`                       | Widens three command storage versions, adds four approval columns/defaults, and rebuilds correlation keys/FKs                                                             | Final declarations are correct; defaults remain necessary for admitted v1 records                                                                                            | Declare final columns, keys, and FKs once                                                                                   |
| `240007_credential_reference_total`                   | Repairs the prior nullable credential reference                                                                                                                           | Correct-choice fix; no final residue                                                                                                                                         | Declare the column total in `model_call`; this file vanishes                                                                |
| `250001_tool_loop`                                    | Adds six tables, widens lifecycle/transcript/outbox state, and installs tool-loop validators                                                                              | New tables and final constraints are tight. History residue is the queued-origin backfill, three validator renames, and a DO block that string-patches a prior function body | Keep the six final tables and final validator bodies; omit UPDATE/trigger-disable choreography and dynamic function surgery |
| `260101_session_metadata`                             | Adds seven metadata/current-state/receipt tables and one command kind                                                                                                     | Pure from-scratch shape apart from extending the command registry function                                                                                                   | Keep as a coherent baseline family with the registry's final definition                                                     |
| `260300_context_frontier_prefixes`                    | Adds prefix identity, backfills longest prefixes, renames the member table to delta, deletes inherited rows, creates a resolved view, and replaces completeness functions | Behavior is correct. The conversion UPDATE/DELETE and rename disappear; physical constraint/trigger names are stale                                                          | Declare header, delta, resolver, and view directly; rename physical artifacts consistently                                  |
| `270001_imported_conversation_source_session_lineage` | Adds nullable exact-byte evidence and a partial non-unique hash index                                                                                                     | NULL is genuine unknown/conflict; the index matches exact grouping                                                                                                           | Fold the column and index into `imported_conversation`                                                                      |
| `280001_parked_approval_interrupt_rejection`          | Replaces submit-result CHECKs, the interrupt correlator, and two trigger predicates                                                                                       | Final typed rejection and parked-phase proof are correct; all 459 lines are reshaping an existing table/function/trigger family                                              | Inline only the final CHECKs, function body, and trigger predicates                                                         |

The tool-loop migration's dynamic `pg_get_functiondef` text replacement is the
strongest reason not to perform a tail-only squash: it is safe only in the exact
historical function chain. A baseline should contain the already-patched final
function body, never reproduce the patch operation.

## Divergence list and squash risks

### 1. Duplicate accepted-result key and foreign key

**Accumulated form.** `180005` added the three-column “general” UNIQUE and FK;
`220001` dropped the older four-column command-result FK and re-added its old
name over the same three columns without removing the `180005` pair.

**Cleaner declaration.** Keep exactly:

```sql
CONSTRAINT submit_input_command_accepted_result_key
    UNIQUE (command_id, result_accepted_input_id, result_session_id)

CONSTRAINT accepted_input_command_result_fk
    FOREIGN KEY (accepting_command_id, accepted_input_id, session_id)
    REFERENCES submit_input_command (
        command_id,
        result_accepted_input_id,
        result_session_id
    )
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED
```

Omit `submit_input_command_general_applied_key` and
`accepted_input_general_command_result_fk`. Keep
`submit_input_command_general_applied_effect_fk`; it is the reverse
effect-correlation FK and is not a duplicate.

**Risk.** No behavioral loss: each duplicate has identical columns and
semantics. Tests currently assert the “general” FK name, so the reset must
update the inventory assertion. Catalog comparison must allow the removal of one
unique index and one FK.

### 2. `context_frontier_delta` carries member-era physical names

**Accumulated form.** The renamed table retains `context_frontier_member_pk`,
`_position_positive_u64`, `_entry_once`, `_frontier_fk`, and `_entry_fk`; its
four triggers and the two validation functions also use
`context_frontier_member_*`.

**Cleaner declaration.** Name physical table artifacts
`context_frontier_delta_*`, including the error constraint names emitted by the
two delta validation functions. Keep `resolve_context_frontier_members` and the
`context_frontier_member` view: those name the logical resolved membership, not
the physical suffix rows.

**Risk.** Several corruption tests assert
`context_frontier_member_within_declared_count`, inspect exact trigger names, or
disable `context_frontier_member_is_append_only`; update those together.
Production SQL names the table and resolver/view but does not name these
constraints. The view is read by production outbox queries and cannot be removed
as part of a migration-only cleanup.

### 3. One misleading live validator name

**Accumulated form.** `require_submit_input_legacy_effect_correlation` is still
load-bearing for every non-interrupt receipt. `280001` adds another trigger
predicate reference to it.

**Cleaner declaration.** Rename it
`require_non_interrupt_submit_input_effect_correlation`. Retain the layered
`assert_*_without_*` helpers: although introduced by successive renames, their
names accurately state the branches each final wrapper excludes.

**Risk.** The function and its one final trigger definition must move
atomically. No current test pins this function name, and there is no stored-data
risk.

### 4. Migration choreography that has no clean-baseline object

Backfills, trigger disable/enable windows, NOT VALID/VALIDATE phases, function
renames, dynamic function surgery, and DROP/re-add CHECK arms are correct
forward-migration techniques but wrong baseline declarations. They occur
throughout `180002`–`220005` and now in `250001`, `260300`, and `280001`.
`200001`, `200006`, `240004`, `240005`, `240007`, `270001`, and `280001` vanish
entirely as files because their final artifacts belong in table-family
declarations.

**Cleaner declaration.** Create every final table, constraint, index, function,
view, and trigger once, in dependency order. Seed only the three live singleton
rows: `hub_fence_state`, `outbox_sequence_state`, and `outbox_delivery_state`.

**Risk.** Do not use the old chain's intermediate-state tests as acceptance
criteria for the new baseline. Use normalized final-catalog comparison plus the
behavior/invariant suite.

### 5. Version and filename scars

The 27-file names retain reserved-slot gaps (`202607200003`–`005`) and
stack-order suffixes (`202607260101`, `260300`). These do not affect the final
catalog but are not a correct-choice baseline inventory.

**Cleaner declaration.** Replace the whole set with checkpoint-dated baseline
versions. Keep two phases rather than one: a minimal first migration installs
the hub fence; the second installs the complete final schema through the
checkpoint. This preserves the real `initialize_hub_fence`/fenced-pool boundary
without preserving subsystem history.

**Risk.** Every existing `_sqlx_migrations` ledger will reject or misinterpret
the rewritten set. The pre-production decision therefore requires recreation,
not repair, of every dev database. `HUB_FENCE_MIGRATION_VERSION` must change to
the new first-baseline version.

### 6. History-specific prose and tests

The schema is correct, but several current verification surfaces make the old
chain observable:

- `crates/persistence/tests/postgres_integration.rs` applies only the first
  three migrations and proves the `180004`/`180005` backfill.
- `crates/persistence/tests/conversation_import_postgres.rs` stops before
  `240005` and `260300`, then applies the tail to prove both forward
  conversions.
- `crates/persistence/src/hub_fence.rs` pins `202607230001`.
- [persistence-protocol](../spec/persistence-protocol.md),
  [identity-and-commands](../spec/identity-and-commands.md),
  [model-call-execution](../spec/model-call-execution.md),
  [sessions-and-transcript](../spec/sessions-and-transcript.md), and
  [tool-loop](../spec/tool-loop.md), and
  [turn-lifecycle-and-scheduling](../spec/turn-lifecycle-and-scheduling.md) cite
  old file counts, versions, or succession. In particular, the tool-loop page
  describes removal of a constraint the clean baseline will never create.
- [conversation-import](../spec/conversation-import.md) includes a
  pre-column-row rationale for nullable lineage. NULL remains correct for
  unknown/conflicting evidence, but that history clause ceases to describe a
  reachable baseline row.

**Cleaner treatment.** Delete conversion-only tests or replace them with direct
final-schema behavior tests only where equivalent coverage does not already
exist. Rewrite specification text to name owning constraints and implemented
behavior rather than the retired sequence. Do not edit historical decision-log
entries.

## In-flight prerequisite

`origin/agent/review-workflow-persistence` adds one 2,331-line migration,
currently named `202607260400_review_workflow.sql`. It creates ten review
workflow tables, adds 29 indexes in total when constraint-backed indexes are
included, adds 33 triggers, adds 12 functions, and replaces a `turn_lifecycle`
CHECK. Applied to the audited catalog, it introduces no new exact duplicate
structural constraint or index; the same two duplicates identified above remain
the only ones.

That branch must land or be abandoned before the checkpoint. Its current
`260400` version is lower than `main`'s `280001` maximum, so it cannot merge
unchanged under the [prefix-order rule](../spec/persistence-protocol.md). The
stacked `origin/agent/signalboxd-rename` branch carries the same migration
through ancestry and adds no second migration. No other unmerged remote branch
based on current work adds a migration beyond files already on `main`.

After the review migration, the expected catalog rises to 58 tables, one view,
178 indexes, 161 triggers, and 100 functions; its schema-only dump is 13,035
lines. That is the checkpoint input the reset should baseline, not today's
27-file catalog.

## Mechanical reset plan

1. Obtain the owner checkpoint call. Freeze migration-bearing branches; verify
   the review-workflow migration has landed or been explicitly abandoned and
   re-scan all unmerged branches for migration files.
2. Apply the frozen old chain to an empty pinned PostgreSQL instance. Capture
   normalized catalogs for tables/columns/defaults, constraints, indexes,
   functions, views, and triggers. Preserve a schema-only dump as a review
   oracle, not as blindly accepted source.
3. Author two checkpoint-dated files:
   - baseline 1: the shared immutable-record and
     `reject_outbox_table_truncate()` helpers plus `hub_fence_state`, its
     guards, and seed row;
   - baseline 2: every remaining final table, singleton seed, constraint,
     independent index, function, view, and trigger in dependency order.
4. In baseline 2, intentionally remove the duplicate UNIQUE/FK, rename
   `context_frontier_delta` physical artifacts, and rename the non-interrupt
   submit validator. Declare final CHECK arms and function bodies directly.
   Perform no UPDATE, DELETE, trigger-disable, NOT VALID, ALTER FUNCTION RENAME,
   or `pg_get_functiondef` surgery.
5. Update `HUB_FENCE_MIGRATION_VERSION` to baseline 1. Rework the three
   migration-prefix helpers/tests named above, constraint/trigger-name
   assertions, and any exact inventory counts. Keep final-behavior tests.
6. Update only the owning specification sections that cite the retired inventory
   or succession, advancing verified-against references when their stated
   implemented behavior is reverified against the baseline.
7. Apply the baseline to a second empty database. Diff normalized catalogs
   against the oracle with an explicit allowlist containing only the duplicate
   removal, physical-name changes, and validator function/trigger rename.
   Confirm the frozen checkpoint inventory and the logical frontier view are
   present. If the review-workflow migration lands, the expected table count is
   58; if it is abandoned, the expected count remains 48 unless other checkpoint
   work changes it.
8. Run the complete repository validation sequence and all ignored
   `postgres-integration` tests. Run a fresh hub bootstrap to exercise baseline
   1, fence advancement, and baseline 2 through the production entry point.
9. Recreate every developer database; never patch `_sqlx_migrations` rows or
   suppress checksum failures. Record the checkpoint in the pull request and
   leave the old files reachable only through git history.

## Final recommendation

Unblock the backlog item to **ready after the review-workflow persistence stack
lands or is explicitly abandoned and the owner declares the checkpoint**. Its
implementation should be a full two-phase baseline reset. The final schema needs
only two small correct-choice cleanups—one duplicate key/FK pair and consistent
delta names—but the migration set has enough interleaved history that a partial
squash would preserve the tax without materially lowering execution risk.
