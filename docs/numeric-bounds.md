# Numeric-bound inventory

This inventory classifies every production or test constant found by the
numeric-bound audit at `a517d8d0f`. A `guard` remains fixed in code because
removing it can break the process itself. A `derived` bound is computed from a
guard. A `config` row names the required deployment field that replaces the
constant; the single spelling `"none"` makes that policy unbounded. A
`not-a-bound` row is a fixed representation fact, and a `test` row exists only
to bound or size a fixture.

The 118 rows partition as 35 guards, 8 derived bounds, 62 configuration
policies, 9 representation facts, and 4 test fixtures. Source locations are
maintained with the implementation slices that move or delete declarations.

## Guards and derived bounds

| Source                                                         | Constant                                    | Tier    | Pathological case prevented or derivation                                                       |
| -------------------------------------------------------------- | ------------------------------------------- | ------- | ----------------------------------------------------------------------------------------------- |
| `crates/process-protocol/src/lib.rs:70`                        | `MAX_FRAME_BYTES`                           | guard   | One wire frame exhausting process memory.                                                       |
| `crates/process-protocol/src/lib.rs:77`                        | `MAX_CONVERSATION_IMPORT_CHUNK_BYTES`       | derived | Derived from `MAX_FRAME_BYTES`.                                                                 |
| `crates/process-protocol/src/lib.rs:81`                        | `MAX_BLOB_CHUNK_BYTES`                      | derived | Derived from `MAX_FRAME_BYTES`.                                                                 |
| `crates/process-protocol/src/lib.rs:85`                        | `MAX_BLOB_READ_BYTES`                       | derived | Derived from `MAX_FRAME_BYTES`.                                                                 |
| `crates/process-protocol/src/lib.rs:102`                       | `MAX_JSON_CONTAINER_DEPTH`                  | guard   | Pathological parser nesting exhausting the stack.                                               |
| `crates/process-protocol/src/lib.rs:106`                       | `MAX_CONTENT_FRAGMENT_BYTES`                | guard   | One fragmented wire value exhausting frame memory.                                              |
| `crates/process-protocol/src/lib.rs:110`                       | `MAX_SESSION_METADATA_TOTAL_UTF8_BYTES`     | guard   | One metadata value exhausting wire-frame and storage-row memory.                                |
| `crates/process-protocol/src/lib.rs:114`                       | `MAX_SESSION_METADATA_INDEXED_UTF8_BYTES`   | guard   | An indexed metadata value exceeding the database index key size.                                |
| `crates/process-protocol/src/lib.rs:142`                       | `MAX_MODEL_ALIAS_CATALOG_ENTRIES`           | guard   | A model-alias catalog exhausting wire-frame memory.                                             |
| `crates/process-protocol/src/lib.rs:146`                       | `MAX_MODEL_CAPABILITY_CATALOG_ENTRIES`      | guard   | A capability catalog exhausting wire-frame memory.                                              |
| `crates/process-protocol/src/lib.rs:154`                       | `MAX_RATE_VERSION_UTF8_BYTES`               | guard   | The wire grammar advertises accepting rate-version text only to this length.                    |
| `crates/process-protocol/src/lib.rs:162`                       | `MAX_REVIEW_ORCHESTRATION_MEMBERS`          | guard   | A review request exhausting wire-frame memory.                                                  |
| `crates/process-protocol/src/lib.rs:5983`                      | `MAX_UTF8_BYTES`                            | guard   | Mirrors the domain runner-working-directory wire grammar.                                       |
| `apps/signalboxd/src/convergence_sweep_runtime.rs:47`          | `MAX_RESPONSE_BYTES`                        | guard   | One buffered provider response exhausting process memory.                                       |
| `apps/signalboxd/src/convergence_sweep_runtime.rs:57`          | `MAX_CREDENTIAL_BYTES`                      | guard   | Credential input exhausting secret-material memory.                                             |
| `apps/signalboxd/src/turn_liveness_runtime.rs:107`             | `QUIESCENT_ROTATION_PAGE_CEILING`           | guard   | A non-converging quiescent-rotation scan loop.                                                  |
| `apps/signalboxd/src/goal_mode.rs:94`                          | `AUTOMATIC_RESUME_INFRASTRUCTURE_RETRIES`   | guard   | Retrying forever against a dead database.                                                       |
| `crates/model-runtime/src/provider_json.rs:11`                 | `PROVIDER_JSON_NESTING_LIMIT`               | guard   | Pathological provider-JSON nesting exhausting the stack.                                        |
| `crates/model-runtime/src/cli_redaction.rs:14`                 | `MAX_PENDING_STREAM_BYTES`                  | guard   | An unterminated pending stream exhausting redaction memory.                                     |
| `crates/model-runtime/src/cli_redaction.rs:16`                 | `MAX_PENDING_RESCAN_BYTES`                  | derived | Derived from `MAX_PENDING_STREAM_BYTES`.                                                        |
| `crates/model-runtime/src/provider_support.rs:12`              | `MAX_BUFFERED_PROVIDER_RESPONSE_BYTES`      | guard   | A buffered provider response exhausting process memory.                                         |
| `crates/model-runtime/src/provider_support.rs:16`              | `MAX_STREAMED_PROVIDER_RESPONSE_BYTES`      | guard   | An unbounded provider stream exhausting process memory.                                         |
| `apps/client/src/chat.rs:37`                                   | `MAX_CHAT_LINE_BYTES`                       | derived | Derived from the frame-derived input guard plus the command prefix.                             |
| `apps/client/src/lib.rs:63`                                    | `MAX_REVIEW_JSON_INPUT_BYTES`               | derived | Derived from `MAX_FRAME_BYTES`.                                                                 |
| `apps/client/src/lib.rs:65`                                    | `MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES`      | derived | Derived from `MAX_FRAME_BYTES`.                                                                 |
| `apps/client/src/arguments.rs:2550`                            | `MAX_UTF8_BYTES`                            | guard   | Mirrors the canonical session-template-name wire grammar.                                       |
| `crates/application/src/repo_watch.rs:69`                      | `MAX_REPO_WATCH_EVENT_IDENTITY_STREAMS`     | guard   | A hostile identity fan-out exhausting resident frontier memory.                                 |
| `crates/application/src/repo_watch.rs:273`                     | `MAX_CHECK_COMPLETION_GENERATION_BYTES`     | guard   | The wire contract advertises accepting provider check-generation text only to this length.      |
| `crates/persistence/src/automatic_reconciliation.rs:38`        | `CLAIM_WINDOW`                              | guard   | A zero-width reconciliation claim expiring before work starts.                                  |
| `crates/tools-code-host/src/code_host/github.rs:49`            | `MAX_JSON_RESPONSE_BYTES`                   | guard   | One buffered provider JSON response exhausting process memory.                                  |
| `crates/tools-code-host/src/code_host/github.rs:59`            | `MAX_REPOSITORY_SYMLINK_TARGET_BYTES`       | guard   | The tool contract advertises accepting symlink targets only to this length.                     |
| `crates/tools-code-host/src/code_host/github.rs:61`            | `MAX_REPOSITORY_SUBMODULE_URL_BYTES`        | guard   | The tool contract advertises accepting submodule URLs only to this length.                      |
| `crates/tools-code-host/src/code_host/github.rs:63`            | `MAX_REPOSITORY_CONTENTS_ENTRY_FIXED_BYTES` | guard   | Fixed entry overhead causing a repository-contents response to exceed buffered response memory. |
| `crates/tools-code-host/src/code_host/github.rs:65`            | `MAX_REPOSITORY_CONTENTS_RESPONSE_BYTES`    | derived | Derived from the fixed entry guard and provider exposure size.                                  |
| `crates/tools-code-host/src/code_host/github.rs:81`            | `MAX_REDIRECT_URL_BYTES`                    | guard   | The client contract advertises accepting redirect URLs only to this length.                     |
| `crates/tools-code-host/src/code_host/result.rs:13`            | `MAX_RESULT_URL_BYTES`                      | guard   | The tool contract advertises accepting result URLs only to this length.                         |
| `crates/tools-code-host/src/code_host/result.rs:17`            | `MAX_ENCODED_RESULT_BYTES`                  | guard   | One encoded tool result exhausting transport memory.                                            |
| `crates/tools-code-host/src/code_host/arguments.rs:9`          | `MAX_REPOSITORY_BYTES`                      | guard   | The tool grammar advertises accepting repository spellings only to this length.                 |
| `crates/tools-code-host/src/code_host/arguments.rs:11`         | `MAX_FILE_PATH_BYTES`                       | guard   | The tool grammar advertises accepting file paths only to this length.                           |
| `crates/tools-code-host/src/code_host/arguments.rs:13`         | `MAX_COMMENT_BODY_BYTES`                    | guard   | The tool grammar advertises accepting comment bodies only to this length.                       |
| `crates/tools-code-host/src/code_host/arguments.rs:15`         | `MAX_OPAQUE_ID_BYTES`                       | guard   | The tool grammar advertises accepting opaque identifiers only to this length.                   |
| `crates/tools-code-host/src/code_host/arguments.rs:17`         | `MAX_CURSOR_BYTES`                          | guard   | The tool grammar advertises accepting pagination cursors only to this length.                   |
| `crates/tools-code-host/src/code_host/repository_result.rs:18` | `MAX_REPOSITORY_FILE_SCAN_BYTES`            | guard   | One ranged repository-file read exhausting process memory.                                      |

## Required configuration policies

| Source                                                         | Constant                                          | Tier   | Required field replacing the constant                                            |
| -------------------------------------------------------------- | ------------------------------------------------- | ------ | -------------------------------------------------------------------------------- |
| `apps/signalboxd/src/repo_watch_runtime.rs:158`                | `REPOSITORY_RECONCILIATION_QUANTUM`               | config | `numeric_bounds.repository_reconciliation_quantum`                               |
| `crates/process-protocol/src/lib.rs:94`                        | `MAX_CONCURRENT_SNAPSHOT_READERS`                 | config | `numeric_bounds.max_concurrent_snapshot_readers`                                 |
| `crates/process-protocol/src/lib.rs:98`                        | `MAX_BLOB_REPLICA_COUNT`                          | config | `numeric_bounds.max_blob_replica_count`                                          |
| `crates/process-protocol/src/lib.rs:118`                       | `MAX_SESSION_METADATA_TAGS`                       | config | `numeric_bounds.max_session_metadata_tags`                                       |
| `crates/process-protocol/src/lib.rs:122`                       | `MAX_SESSION_METADATA_ATTRIBUTES`                 | config | `numeric_bounds.max_session_metadata_attributes`                                 |
| `crates/process-protocol/src/lib.rs:126`                       | `MAX_SESSION_METADATA_REQUIRED_TAGS`              | config | `numeric_bounds.max_session_metadata_required_tags`                              |
| `crates/process-protocol/src/lib.rs:130`                       | `MAX_SYSTEM_PROMPT_UTF8_BYTES`                    | config | `numeric_bounds.max_system_prompt_utf8_bytes`                                    |
| `crates/process-protocol/src/lib.rs:138`                       | `MAX_IMPORTED_TEXT_PREVIEW_UTF8_BYTES`            | config | `numeric_bounds.max_imported_text_preview_utf8_bytes`                            |
| `crates/process-protocol/src/lib.rs:158`                       | `MAX_REVIEW_ORCHESTRATION_CONCERNS`               | config | `numeric_bounds.max_review_orchestration_concerns`                               |
| `crates/process-protocol/src/lib.rs:2537`                      | `MAX_IMPORTED_CONVERSATION_DISPLAY_TITLE_SCALARS` | config | `numeric_bounds.max_imported_conversation_display_title_scalars`                 |
| `apps/signalboxd/src/main.rs:83`                               | `GRACEFUL_SHUTDOWN_CLEANUP_WINDOW`                | config | `numeric_bounds.graceful_shutdown_cleanup_window`                                |
| `apps/signalboxd/src/lib.rs:1012`                              | `EXPIRED_PASS_RECOVERY_ATTEMPTS`                  | config | `numeric_bounds.expired_pass_recovery_attempts`                                  |
| `apps/signalboxd/src/lib.rs:1021`                              | `EXPIRED_PASS_RECOVERY_ATTEMPT_BOUND`             | config | `numeric_bounds.expired_pass_recovery_attempt_bound`                             |
| `apps/signalboxd/src/lib.rs:1030`                              | `EXPIRED_PASS_RECOVERY_LOCK_RETRY_DELAY`          | config | `numeric_bounds.expired_pass_recovery_lock_retry_delay`                          |
| `apps/signalboxd/src/lib.rs:1034`                              | `EXPIRED_PASS_RECOVERY_CONSERVATIVE_RETRY_DELAY`  | config | `numeric_bounds.expired_pass_recovery_conservative_retry_delay`                  |
| `apps/signalboxd/src/convergence_sweep_runtime.rs:45`          | `REQUEST_TIMEOUT`                                 | config | `numeric_bounds.convergence_sweep_request_timeout`                               |
| `apps/signalboxd/src/convergence_sweep_runtime.rs:49`          | `MAX_CONNECTION_PAGES`                            | config | `numeric_bounds.max_convergence_sweep_connection_pages`                          |
| `apps/signalboxd/src/convergence_sweep_runtime.rs:51`          | `MAX_CONCURRENT_TARGETS`                          | config | `numeric_bounds.max_concurrent_convergence_sweep_targets`                        |
| `apps/signalboxd/src/convergence_sweep_runtime.rs:53`          | `MAX_REQUEST_ATTEMPTS`                            | config | `numeric_bounds.max_convergence_sweep_request_attempts`                          |
| `apps/signalboxd/src/convergence_sweep_runtime.rs:55`          | `REQUEST_RETRY_DELAY`                             | config | `numeric_bounds.convergence_sweep_request_retry_delay`                           |
| `apps/signalboxd/src/convergence_sweep_runtime.rs:59`          | `RETRY_BACKOFF_BASE`                              | config | `numeric_bounds.convergence_sweep_retry_backoff_base`                            |
| `apps/signalboxd/src/convergence_sweep_runtime.rs:61`          | `RETRY_BACKOFF_CAP`                               | config | `numeric_bounds.convergence_sweep_retry_backoff_cap`                             |
| `apps/signalboxd/src/turn_liveness_runtime.rs:86`              | `TERMINALIZATIONS_PER_SCAN`                       | config | `numeric_bounds.terminalizations_per_liveness_scan`                              |
| `apps/signalboxd/src/turn_liveness_runtime.rs:115`             | `RECOVERY_ATTEMPT_BOUND`                          | config | `numeric_bounds.turn_liveness_recovery_attempt_bound`                            |
| `apps/signalboxd/src/turn_liveness_runtime.rs:123`             | `AUTOMATIC_RECONCILIATIONS_PER_SCAN`              | config | `numeric_bounds.automatic_reconciliations_per_liveness_scan`                     |
| `apps/signalboxd/src/configuration.rs:477`                     | `MAX_CONVERGENCE_SWEEP_TARGETS`                   | config | `numeric_bounds.max_convergence_sweep_targets`                                   |
| `apps/signalboxd/src/configuration.rs:479`                     | `MAX_CONVERGENCE_SWEEP_INTERVAL`                  | config | `numeric_bounds.max_convergence_sweep_interval`                                  |
| `apps/signalboxd/src/configuration.rs:481`                     | `MAX_CONVERGENCE_SWEEP_COOL_OFF`                  | config | `numeric_bounds.max_convergence_sweep_cool_off`                                  |
| `apps/signalboxd/src/goal_mode.rs:77`                          | `AUTOMATIC_RESUME_BASE_BACKOFF`                   | config | `numeric_bounds.automatic_resume_base_backoff`                                   |
| `apps/signalboxd/src/goal_mode.rs:80`                          | `AUTOMATIC_RESUME_BACKOFF_CAP`                    | config | `numeric_bounds.automatic_resume_backoff_cap`                                    |
| `apps/signalboxd/src/goal_mode.rs:87`                          | `AUTOMATIC_RESUME_ATTEMPT_BUDGET`                 | config | `numeric_bounds.automatic_resume_attempt_budget`                                 |
| `apps/signalboxd/src/goal_mode.rs:101`                         | `AUTOMATIC_RESUME_STARTUP_RETRY_DELAY`            | config | `numeric_bounds.automatic_resume_startup_retry_delay`                            |
| `crates/model-runtime/src/cli_process.rs:1565`                 | `POST_KILL_REAP_BOUND`                            | config | `numeric_bounds.post_kill_reap_bound`                                            |
| `crates/application/src/turn_liveness.rs:23`                   | `STALE_ACTIVE_TURN_BOUND`                         | config | `numeric_bounds.stale_active_turn_bound`                                         |
| `crates/application/src/turn_liveness.rs:26`                   | `BASELINE_TURN_LIVENESS_SCAN_INTERVAL`            | config | `numeric_bounds.turn_liveness_scan_interval`                                     |
| `crates/application/src/turn_liveness.rs:29`                   | `AUTOMATIC_RECONCILIATION_BASE_BACKOFF`           | config | `numeric_bounds.automatic_reconciliation_base_backoff`                           |
| `crates/application/src/turn_liveness.rs:32`                   | `AUTOMATIC_RECONCILIATION_BACKOFF_CAP`            | config | `numeric_bounds.automatic_reconciliation_backoff_cap`                            |
| `crates/application/src/turn_liveness.rs:35`                   | `AUTOMATIC_RECONCILIATION_ATTEMPT_BUDGET`         | config | `numeric_bounds.automatic_reconciliation_attempt_budget`                         |
| `apps/client/src/chat.rs:39`                                   | `TERMINAL_INPUT_CHANNEL_CAPACITY`                 | config | `numeric_bounds.terminal_input_channel_capacity`                                 |
| `apps/client/src/lib.rs:61`                                    | `MAX_INPUT_CONTENT_BYTES`                         | config | `numeric_bounds.max_message_utf8_bytes` learned over the daemon connection.      |
| `apps/client/src/lib.rs:70`                                    | `MIN_METADATA_PAGE_SIZE`                          | config | `numeric_bounds.min_metadata_page_size` learned over the daemon connection.      |
| `apps/client/src/lib.rs:73`                                    | `MAX_METADATA_PAGE_SIZE`                          | config | `numeric_bounds.max_metadata_page_size` learned over the daemon connection.      |
| `apps/client/src/lib.rs:76`                                    | `MAX_REVIEW_FINDINGS_PER_RUN`                     | config | `numeric_bounds.max_review_findings_per_run` learned over the daemon connection. |
| `crates/application/src/model_execution.rs:18`                 | `MAX_AUTOMATIC_TOOL_ROUNDS_PER_TURN`              | config | `numeric_bounds.max_automatic_tool_rounds_per_turn`                              |
| `crates/application/src/session_metadata.rs:234`               | `MAX_REQUIRED_TAGS`                               | config | `numeric_bounds.max_required_tags`                                               |
| `crates/application/src/submit_input.rs:67`                    | `MAX_CONTENT_UTF8_BYTES`                          | config | `numeric_bounds.max_message_utf8_bytes`                                          |
| `crates/application/src/scheduler.rs:40`                       | `BASELINE_RECONCILIATION_SWEEP_INTERVAL`          | config | `numeric_bounds.reconciliation_sweep_interval`                                   |
| `crates/application/src/scheduler.rs:42`                       | `BASELINE_NUDGE_BUFFER_CAPACITY`                  | config | `numeric_bounds.nudge_buffer_capacity`                                           |
| `crates/application/src/scheduler.rs:48`                       | `SCHEDULER_PASS_ADMISSION_CAP`                    | config | `numeric_bounds.scheduler_pass_admission_cap`                                    |
| `crates/application/src/scheduler.rs:51`                       | `SCHEDULER_PASS_OCCUPANCY_BOUND`                  | config | `numeric_bounds.scheduler_pass_occupancy_bound`                                  |
| `crates/model-runtime/src/redaction.rs:15`                     | `MAX_NATIVE_MESSAGE_BYTES`                        | config | `numeric_bounds.max_native_message_bytes`                                        |
| `crates/persistence/src/turn_liveness.rs:55`                   | `TERMINALIZATION_LOCK_WAIT`                       | config | `numeric_bounds.terminalization_lock_wait`                                       |
| `crates/persistence/src/turn_liveness.rs:65`                   | `TERMINALIZATION_ACQUIRE_WAIT`                    | config | `numeric_bounds.terminalization_acquire_wait`                                    |
| `crates/persistence/src/turn_liveness.rs:84`                   | `TERMINALIZATION_WRITE_LOCK_WAIT`                 | config | `numeric_bounds.terminalization_write_lock_wait`                                 |
| `crates/persistence/src/lib.rs:328`                            | `DISPOSABLE_POSTGRES_STATE_CEILING_BYTES`         | config | `numeric_bounds.disposable_postgres_state_ceiling_bytes`                         |
| `crates/model-provider-runtime/src/lib.rs:60`                  | `DIAGNOSTIC_MODEL_IDENTITY_LIMIT`                 | config | `numeric_bounds.diagnostic_model_identity_limit`                                 |
| `crates/tools-code-host/src/code_host/github.rs:47`            | `DEFAULT_TIMEOUT`                                 | config | `numeric_bounds.code_host_request_timeout`                                       |
| `crates/tools-code-host/src/code_host/github.rs:79`            | `MAX_JOB_LOG_BYTES`                               | config | `numeric_bounds.max_job_log_bytes`                                               |
| `crates/tools-code-host/src/code_host/github.rs:86`            | `MAX_STACK_COMPARISONS_IN_FLIGHT`                 | config | `numeric_bounds.max_stack_comparisons_in_flight`                                 |
| `crates/tools-code-host/src/code_host/result.rs:11`            | `MAX_RESULT_TEXT_BYTES`                           | config | `numeric_bounds.max_code_host_result_text_bytes`                                 |
| `crates/tools-code-host/src/code_host/result.rs:15`            | `MAX_RESULT_ITEMS`                                | config | `numeric_bounds.max_code_host_result_items`                                      |
| `crates/tools-code-host/src/code_host/repository_result.rs:15` | `MAX_REPOSITORY_FILE_CONTENT_BYTES`               | config | `numeric_bounds.max_repository_file_content_bytes`                               |

## Representation facts

| Source                                                         | Constant                                        | Tier        | Why it is not a bound                                   |
| -------------------------------------------------------------- | ----------------------------------------------- | ----------- | ------------------------------------------------------- |
| `crates/process-protocol/src/lib.rs:150`                       | `MAX_DOLLAR_AMOUNT_BYTES`                       | not-a-bound | Longest canonical `rust_decimal` spelling.              |
| `crates/process-protocol/src/lib.rs:1058`                      | `MAX_DECIMAL_COEFFICIENT`                       | not-a-bound | Fixed `rust_decimal` coefficient representation.        |
| `apps/client/src/arguments.rs:2656`                            | `MAXIMUM_REVIEW_CONFIDENCE_BASIS_POINTS`        | not-a-bound | Fixed full-scale basis-point representation.            |
| `crates/tools-code-host/src/code_host/github.rs:51`            | `MAX_JSON_ESCAPE_BYTES_PER_SOURCE_BYTE`         | not-a-bound | Fixed maximum JSON escape expansion.                    |
| `crates/tools-code-host/src/code_host/github.rs:57`            | `MAX_REPOSITORY_CONTENTS_PATH_FIELDS_PER_ENTRY` | not-a-bound | Fixed provider response field count.                    |
| `crates/tools-code-host/src/code_host/github.rs:75`            | `MAX_COMMIT_SHA_RESPONSE_BYTES`                 | not-a-bound | A fixed-width commit SHA plus one optional newline.     |
| `crates/tools-code-host/src/code_host/github.rs:84`            | `MAX_CHANGED_FILE_PAGES`                        | not-a-bound | Fixed provider exposure divided by its fixed page size. |
| `crates/tools-code-host/src/code_host/repository_result.rs:21` | `MAX_OBSERVED_DIRECTORY_ENTRIES`                | not-a-bound | Fixed provider contents-endpoint exposure.              |
| `crates/tools-code-host/src/code_host/repository_result.rs:23` | `MAX_UTF8_BOUNDARY_DISCARD_BYTES`               | not-a-bound | Fixed maximum UTF-8 continuation width.                 |

## Test fixtures

| Source                                                   | Constant                            | Tier | Fixture role                                                |
| -------------------------------------------------------- | ----------------------------------- | ---- | ----------------------------------------------------------- |
| `apps/signalboxd/tests/process_protocol_runtime.rs:2245` | `FLEET_OCCUPANCY_BOUND`             | test | Exercises production recovery promptly.                     |
| `apps/signalboxd/tests/process_protocol_runtime.rs:2247` | `FLEET_ASSERTION_BOUND`             | test | Keeps each fault probe within one CI minute.                |
| `apps/signalboxd/tests/process_protocol_runtime.rs:2249` | `FLEET_SETUP_BOUND`                 | test | Admits a full contended fleet within two CI minutes.        |
| `crates/tools-git/src/tests/layout.rs:42`                | `WIDE_ADMINISTRATIVE_SIBLING_COUNT` | test | Exceeds the dogfood supervisor's former descriptor ceiling. |
