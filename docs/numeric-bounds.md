# Numeric-bound inventory

This inventory classifies every production or test constant found by the
numeric-bound audit at `a517d8d0f`. A `guard` remains fixed in code because
removing it can break the process itself. A `derived` bound is computed from a
guard. A `config` row names the required deployment field that replaces the
constant; the single spelling `"none"` makes that policy unbounded. A
`not-a-bound` row is a fixed representation fact, and a `test` row exists only
to bound or size a fixture.

The live-line merge adds one model-exchange policy to the original audit and the
automatic-resume lifetime ceiling one goal policy; the liveness watchdog's
single recovery-attempt constant now answers to two configured policies because
its two consumers need different ceilings. The 128 rows partition as 36 guards,
8 derived bounds, 70 configuration policies, 10 representation facts, and 4 test
fixtures. Source locations are maintained with the implementation slices that
move or delete declarations.

The numeric-bound gate accepts only structural guards, values mechanically
derived from guards, representation facts, and test fixtures from this
classified set. Exact pre-existing daemon and persistence candidates omitted
from the commissioned audit remain outside the blocking set; any newly named
bound in either root fails closed.

## Guards and derived bounds

| Source                                                         | Constant                                    | Tier    | Pathological case prevented or derivation                                                       |
| -------------------------------------------------------------- | ------------------------------------------- | ------- | ----------------------------------------------------------------------------------------------- |
| `crates/process-protocol/src/lib.rs:71`                        | `MAX_FRAME_BYTES`                           | guard   | One wire frame exhausting process memory.                                                       |
| `crates/process-protocol/src/lib.rs:78`                        | `MAX_CONVERSATION_IMPORT_CHUNK_BYTES`       | derived | Derived from `MAX_FRAME_BYTES`.                                                                 |
| `crates/process-protocol/src/lib.rs:82`                        | `MAX_BLOB_CHUNK_BYTES`                      | derived | Derived from `MAX_FRAME_BYTES`.                                                                 |
| `crates/process-protocol/src/lib.rs:86`                        | `MAX_BLOB_READ_BYTES`                       | derived | Derived from `MAX_FRAME_BYTES`.                                                                 |
| `crates/process-protocol/src/lib.rs:89`                        | `MAX_JSON_CONTAINER_DEPTH`                  | guard   | Pathological parser nesting exhausting the stack.                                               |
| `crates/process-protocol/src/lib.rs:93`                        | `MAX_CONTENT_FRAGMENT_BYTES`                | guard   | One fragmented wire value exhausting frame memory.                                              |
| `crates/process-protocol/src/lib.rs:97`                        | `MAX_SESSION_METADATA_TOTAL_UTF8_BYTES`     | guard   | One metadata value exhausting wire-frame and storage-row memory.                                |
| `crates/process-protocol/src/lib.rs:101`                       | `MAX_SESSION_METADATA_INDEXED_UTF8_BYTES`   | guard   | An indexed metadata value exceeding the database index key size.                                |
| `crates/process-protocol/src/lib.rs:105`                       | `MAX_MODEL_ALIAS_CATALOG_ENTRIES`           | guard   | A model-alias catalog exhausting wire-frame memory.                                             |
| `crates/process-protocol/src/lib.rs:109`                       | `MAX_MODEL_CAPABILITY_CATALOG_ENTRIES`      | guard   | A capability catalog exhausting wire-frame memory.                                              |
| `crates/process-protocol/src/lib.rs:117`                       | `MAX_RATE_VERSION_UTF8_BYTES`               | guard   | The wire grammar advertises accepting rate-version text only to this length.                    |
| `crates/process-protocol/src/lib.rs:121`                       | `MAX_REVIEW_ORCHESTRATION_MEMBERS`          | guard   | A review request exhausting wire-frame memory.                                                  |
| `crates/process-protocol/src/lib.rs:5902`                      | `MAX_UTF8_BYTES`                            | guard   | Mirrors the domain runner-working-directory wire grammar.                                       |
| `apps/signalboxd/src/convergence_sweep_runtime.rs:47`          | `MAX_RESPONSE_BYTES`                        | guard   | One buffered provider response exhausting process memory.                                       |
| `apps/signalboxd/src/convergence_sweep_runtime.rs:57`          | `MAX_CREDENTIAL_BYTES`                      | guard   | Credential input exhausting secret-material memory.                                             |
| `apps/signalboxd/src/turn_liveness_runtime.rs:108`             | `QUIESCENT_ROTATION_PAGE_CEILING`           | guard   | A non-converging quiescent-rotation scan loop.                                                  |
| `apps/signalboxd/src/goal_mode.rs:95`                          | `AUTOMATIC_RESUME_INFRASTRUCTURE_RETRIES`   | guard   | Retrying forever against a dead database.                                                       |
| `crates/model-runtime/src/provider_json.rs:11`                 | `PROVIDER_JSON_NESTING_LIMIT`               | guard   | Pathological provider-JSON nesting exhausting the stack.                                        |
| `crates/model-runtime/src/cli_redaction.rs:14`                 | `MAX_PENDING_STREAM_BYTES`                  | guard   | An unterminated pending stream exhausting redaction memory.                                     |
| `crates/model-runtime/src/cli_redaction.rs:16`                 | `MAX_PENDING_RESCAN_BYTES`                  | derived | Derived from `MAX_PENDING_STREAM_BYTES`.                                                        |
| `crates/model-runtime/src/provider_support.rs:12`              | `MAX_BUFFERED_PROVIDER_RESPONSE_BYTES`      | guard   | A buffered provider response exhausting process memory.                                         |
| `crates/model-runtime/src/provider_support.rs:16`              | `MAX_STREAMED_PROVIDER_RESPONSE_BYTES`      | guard   | An unbounded provider stream exhausting process memory.                                         |
| `apps/client/src/chat.rs:38`                                   | `MAX_CHAT_LINE_BYTES`                       | derived | Derived from `MAX_FRAME_BYTES` plus the command prefix.                                         |
| `apps/client/src/lib.rs:65`                                    | `MAX_REVIEW_JSON_INPUT_BYTES`               | derived | Derived from `MAX_FRAME_BYTES`.                                                                 |
| `apps/client/src/lib.rs:67`                                    | `MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES`      | derived | Derived from `MAX_FRAME_BYTES`.                                                                 |
| `apps/client/src/arguments.rs:2543`                            | `MAX_UTF8_BYTES`                            | guard   | Mirrors the canonical session-template-name wire grammar.                                       |
| `crates/application/src/repo_watch.rs:69`                      | `MAX_REPO_WATCH_EVENT_IDENTITY_STREAMS`     | guard   | A hostile identity fan-out exhausting resident frontier memory.                                 |
| `crates/application/src/repo_watch.rs:273`                     | `MAX_CHECK_COMPLETION_GENERATION_BYTES`     | guard   | The wire contract advertises accepting provider check-generation text only to this length.      |
| `crates/persistence/src/automatic_reconciliation.rs:39`        | `CLAIM_WINDOW`                              | guard   | A zero-width reconciliation claim expiring before work starts.                                  |
| `crates/tools-code-host/src/code_host/github.rs:47`            | `MAX_JSON_RESPONSE_BYTES`                   | guard   | One buffered provider JSON response exhausting process memory.                                  |
| `crates/tools-code-host/src/code_host/github.rs:57`            | `MAX_REPOSITORY_SYMLINK_TARGET_BYTES`       | guard   | The tool contract advertises accepting symlink targets only to this length.                     |
| `crates/tools-code-host/src/code_host/github.rs:59`            | `MAX_REPOSITORY_SUBMODULE_URL_BYTES`        | guard   | The tool contract advertises accepting submodule URLs only to this length.                      |
| `crates/tools-code-host/src/code_host/github.rs:61`            | `MAX_REPOSITORY_CONTENTS_ENTRY_FIXED_BYTES` | guard   | Fixed entry overhead causing a repository-contents response to exceed buffered response memory. |
| `crates/tools-code-host/src/code_host/github.rs:63`            | `MAX_REPOSITORY_CONTENTS_RESPONSE_BYTES`    | derived | Derived from the fixed entry guard and provider exposure size.                                  |
| `crates/tools-code-host/src/code_host/github.rs:77`            | `MAX_REDIRECT_URL_BYTES`                    | guard   | The client contract advertises accepting redirect URLs only to this length.                     |
| `crates/tools-code-host/src/code_host/result.rs:15`            | `MAX_RESULT_URL_BYTES`                      | guard   | The tool contract advertises accepting result URLs only to this length.                         |
| `crates/tools-code-host/src/code_host/result.rs:17`            | `MAX_ENCODED_RESULT_BYTES`                  | guard   | One encoded tool result exhausting transport memory.                                            |
| `crates/tools-code-host/src/code_host/arguments.rs:9`          | `MAX_REPOSITORY_BYTES`                      | guard   | The tool grammar advertises accepting repository spellings only to this length.                 |
| `crates/tools-code-host/src/code_host/arguments.rs:11`         | `MAX_FILE_PATH_BYTES`                       | guard   | The tool grammar advertises accepting file paths only to this length.                           |
| `crates/tools-code-host/src/code_host/arguments.rs:13`         | `MAX_COMMENT_BODY_BYTES`                    | guard   | The tool grammar advertises accepting comment bodies only to this length.                       |
| `crates/tools-code-host/src/code_host/arguments.rs:15`         | `MAX_OPAQUE_ID_BYTES`                       | guard   | The tool grammar advertises accepting opaque identifiers only to this length.                   |
| `crates/tools-code-host/src/code_host/arguments.rs:17`         | `MAX_CURSOR_BYTES`                          | guard   | The tool grammar advertises accepting pagination cursors only to this length.                   |
| `crates/tools-code-host/src/code_host/repository_result.rs:15` | `MAX_REPOSITORY_FILE_SCAN_BYTES`            | guard   | One ranged repository-file read exhausting process memory.                                      |
| `crates/persistence/src/lifecycle_metrics.rs:22`               | `MAX_REPORTED_WEEKS`                        | guard   | Weeks accrue forever, so one metric report exhausting wire-frame memory.                        |

## Required configuration policies

| Source                               | Constant                                          | Tier   | Required field replacing the constant                                                |
| ------------------------------------ | ------------------------------------------------- | ------ | ------------------------------------------------------------------------------------ |
| `config/signalboxd.example.toml:24`  | `REPOSITORY_RECONCILIATION_QUANTUM`               | config | `numeric_bounds.repository_reconciliation_quantum`                                   |
| `config/signalboxd.example.toml:26`  | `MAX_CONCURRENT_SNAPSHOT_READERS`                 | config | `numeric_bounds.max_concurrent_snapshot_readers`                                     |
| `config/signalboxd.example.toml:28`  | `MAX_BLOB_REPLICA_COUNT`                          | config | `numeric_bounds.max_blob_replica_count`                                              |
| `config/signalboxd.example.toml:30`  | `MAX_SESSION_METADATA_TAGS`                       | config | `numeric_bounds.max_session_metadata_tags`                                           |
| `config/signalboxd.example.toml:32`  | `MAX_SESSION_METADATA_ATTRIBUTES`                 | config | `numeric_bounds.max_session_metadata_attributes`                                     |
| `config/signalboxd.example.toml:34`  | `MAX_SESSION_METADATA_REQUIRED_TAGS`              | config | `numeric_bounds.max_session_metadata_required_tags`                                  |
| `config/signalboxd.example.toml:36`  | `MAX_SYSTEM_PROMPT_UTF8_BYTES`                    | config | `numeric_bounds.max_system_prompt_utf8_bytes`                                        |
| `config/signalboxd.example.toml:38`  | `MAX_IMPORTED_TEXT_PREVIEW_UTF8_BYTES`            | config | `numeric_bounds.max_imported_text_preview_utf8_bytes`                                |
| `config/signalboxd.example.toml:40`  | `MAX_REVIEW_ORCHESTRATION_CONCERNS`               | config | `numeric_bounds.max_review_orchestration_concerns`                                   |
| `config/signalboxd.example.toml:42`  | `MAX_IMPORTED_CONVERSATION_DISPLAY_TITLE_SCALARS` | config | `numeric_bounds.max_imported_conversation_display_title_scalars`                     |
| `config/signalboxd.example.toml:44`  | `GRACEFUL_SHUTDOWN_CLEANUP_WINDOW`                | config | `numeric_bounds.graceful_shutdown_cleanup_window`                                    |
| `config/signalboxd.example.toml:46`  | `DEFAULT_MODEL_EXCHANGE_TIMEOUT`                  | config | `numeric_bounds.model_exchange_timeout`                                              |
| `config/signalboxd.example.toml:48`  | `EXPIRED_PASS_RECOVERY_ATTEMPTS`                  | config | `numeric_bounds.expired_pass_recovery_attempts`                                      |
| `config/signalboxd.example.toml:50`  | `EXPIRED_PASS_RECOVERY_ATTEMPT_BOUND`             | config | `numeric_bounds.expired_pass_recovery_attempt_bound`                                 |
| `config/signalboxd.example.toml:52`  | `EXPIRED_PASS_RECOVERY_LOCK_RETRY_DELAY`          | config | `numeric_bounds.expired_pass_recovery_lock_retry_delay`                              |
| `config/signalboxd.example.toml:54`  | `EXPIRED_PASS_RECOVERY_CONSERVATIVE_RETRY_DELAY`  | config | `numeric_bounds.expired_pass_recovery_conservative_retry_delay`                      |
| `config/signalboxd.example.toml:56`  | `REQUEST_TIMEOUT`                                 | config | `numeric_bounds.convergence_sweep_request_timeout`                                   |
| `config/signalboxd.example.toml:58`  | `MAX_CONNECTION_PAGES`                            | config | `numeric_bounds.max_convergence_sweep_connection_pages`                              |
| `config/signalboxd.example.toml:60`  | `MAX_CONCURRENT_TARGETS`                          | config | `numeric_bounds.max_concurrent_convergence_sweep_targets`                            |
| `config/signalboxd.example.toml:62`  | `MAX_REQUEST_ATTEMPTS`                            | config | `numeric_bounds.max_convergence_sweep_request_attempts`                              |
| `config/signalboxd.example.toml:64`  | `REQUEST_RETRY_DELAY`                             | config | `numeric_bounds.convergence_sweep_request_retry_delay`                               |
| `config/signalboxd.example.toml:66`  | `RETRY_BACKOFF_BASE`                              | config | `numeric_bounds.convergence_sweep_retry_backoff_base`                                |
| `config/signalboxd.example.toml:68`  | `RETRY_BACKOFF_CAP`                               | config | `numeric_bounds.convergence_sweep_retry_backoff_cap`                                 |
| `config/signalboxd.example.toml:70`  | `TERMINALIZATIONS_PER_SCAN`                       | config | `numeric_bounds.terminalizations_per_liveness_scan`                                  |
| `config/signalboxd.example.toml:72`  | `RECOVERY_ATTEMPT_BOUND`                          | config | `numeric_bounds.turn_liveness_recovery_attempt_bound`                                |
| `config/signalboxd.example.toml:74`  | `AUTOMATIC_RECONCILIATIONS_PER_SCAN`              | config | `numeric_bounds.automatic_reconciliations_per_liveness_scan`                         |
| `config/signalboxd.example.toml:86`  | `RECOVERY_ATTEMPT_BOUND`                          | config | `numeric_bounds.automatic_reconciliation_attempt_bound`                              |
| `config/signalboxd.example.toml:76`  | `MAX_CONVERGENCE_SWEEP_TARGETS`                   | config | `numeric_bounds.max_convergence_sweep_targets`                                       |
| `config/signalboxd.example.toml:78`  | `MAX_CONVERGENCE_SWEEP_INTERVAL`                  | config | `numeric_bounds.max_convergence_sweep_interval`                                      |
| `config/signalboxd.example.toml:80`  | `MAX_CONVERGENCE_SWEEP_COOL_OFF`                  | config | `numeric_bounds.max_convergence_sweep_cool_off`                                      |
| `config/signalboxd.example.toml:82`  | `AUTOMATIC_RESUME_BASE_BACKOFF`                   | config | `numeric_bounds.automatic_resume_base_backoff`                                       |
| `config/signalboxd.example.toml:84`  | `AUTOMATIC_RESUME_BACKOFF_CAP`                    | config | `numeric_bounds.automatic_resume_backoff_cap`                                        |
| `config/signalboxd.example.toml:86`  | `AUTOMATIC_RESUME_ATTEMPT_BUDGET`                 | config | `numeric_bounds.automatic_resume_attempt_budget`                                     |
| `config/signalboxd.example.toml:98`  | `AUTOMATIC_RESUME_ATTEMPT_CEILING`                | config | `numeric_bounds.automatic_resume_attempt_ceiling`                                    |
| `config/signalboxd.example.toml:88`  | `AUTOMATIC_RESUME_STARTUP_RETRY_DELAY`            | config | `numeric_bounds.automatic_resume_startup_retry_delay`                                |
| `config/signalboxd.example.toml:90`  | `POST_KILL_REAP_BOUND`                            | config | `numeric_bounds.post_kill_reap_bound`                                                |
| `config/signalboxd.example.toml:92`  | `STALE_ACTIVE_TURN_BOUND`                         | config | `numeric_bounds.stale_active_turn_bound`                                             |
| `config/signalboxd.example.toml:94`  | `BASELINE_TURN_LIVENESS_SCAN_INTERVAL`            | config | `numeric_bounds.turn_liveness_scan_interval`                                         |
| `config/signalboxd.example.toml:96`  | `AUTOMATIC_RECONCILIATION_BASE_BACKOFF`           | config | `numeric_bounds.automatic_reconciliation_base_backoff`                               |
| `config/signalboxd.example.toml:98`  | `AUTOMATIC_RECONCILIATION_BACKOFF_CAP`            | config | `numeric_bounds.automatic_reconciliation_backoff_cap`                                |
| `config/signalboxd.example.toml:100` | `AUTOMATIC_RECONCILIATION_ATTEMPT_BUDGET`         | config | `numeric_bounds.automatic_reconciliation_attempt_budget`                             |
| `config/signalboxd.example.toml:102` | `TERMINAL_INPUT_CHANNEL_CAPACITY`                 | config | `numeric_bounds.terminal_input_channel_capacity` learned over the daemon connection. |
| `config/signalboxd.example.toml:104` | `MAX_INPUT_CONTENT_BYTES`                         | config | `numeric_bounds.max_message_utf8_bytes` learned over the daemon connection.          |
| `config/signalboxd.example.toml:106` | `MIN_METADATA_PAGE_SIZE`                          | config | `numeric_bounds.min_metadata_page_size` learned over the daemon connection.          |
| `config/signalboxd.example.toml:108` | `MAX_METADATA_PAGE_SIZE`                          | config | `numeric_bounds.max_metadata_page_size` learned over the daemon connection.          |
| `config/signalboxd.example.toml:110` | `MAX_REVIEW_FINDINGS_PER_RUN`                     | config | `numeric_bounds.max_review_findings_per_run` learned over the daemon connection.     |
| `config/signalboxd.example.toml:112` | `MAX_AUTOMATIC_TOOL_ROUNDS_PER_TURN`              | config | `numeric_bounds.max_automatic_tool_rounds_per_turn`                                  |
| `config/signalboxd.example.toml:128` | `MAX_SAME_CREDENTIAL_ATTEMPTS_PER_TURN`           | config | `numeric_bounds.max_same_credential_attempts_per_turn`                               |
| `config/signalboxd.example.toml:130` | `MAX_REQUIRED_TAGS`                               | config | `numeric_bounds.max_required_tags`                                                   |
| `config/signalboxd.example.toml:118` | `MAX_CONTENT_UTF8_BYTES`                          | config | `numeric_bounds.max_message_utf8_bytes`                                              |
| `config/signalboxd.example.toml:132` | `BASELINE_RECONCILIATION_SWEEP_INTERVAL`          | config | `numeric_bounds.reconciliation_sweep_interval`                                       |
| `config/signalboxd.example.toml:134` | `BASELINE_NUDGE_BUFFER_CAPACITY`                  | config | `numeric_bounds.nudge_buffer_capacity`                                               |
| `config/signalboxd.example.toml:136` | `SCHEDULER_PASS_ADMISSION_CAP`                    | config | `numeric_bounds.scheduler_pass_admission_cap`                                        |
| `config/signalboxd.example.toml:138` | `SCHEDULER_PASS_OCCUPANCY_BOUND`                  | config | `numeric_bounds.scheduler_pass_occupancy_bound`                                      |
| `config/signalboxd.example.toml:140` | `MAX_NATIVE_MESSAGE_BYTES`                        | config | `numeric_bounds.max_native_message_bytes`                                            |
| `config/signalboxd.example.toml:142` | `TERMINALIZATION_LOCK_WAIT`                       | config | `numeric_bounds.terminalization_lock_wait`                                           |
| `config/signalboxd.example.toml:144` | `TERMINALIZATION_ACQUIRE_WAIT`                    | config | `numeric_bounds.terminalization_acquire_wait`                                        |
| `config/signalboxd.example.toml:146` | `TERMINALIZATION_WRITE_LOCK_WAIT`                 | config | `numeric_bounds.terminalization_write_lock_wait`                                     |
| `config/signalboxd.example.toml:148` | `DISPOSABLE_POSTGRES_STATE_CEILING_BYTES`         | config | `numeric_bounds.disposable_postgres_state_ceiling_bytes`                             |
| `config/signalboxd.example.toml:150` | `DIAGNOSTIC_MODEL_IDENTITY_LIMIT`                 | config | `numeric_bounds.diagnostic_model_identity_limit`                                     |
| `config/signalboxd.example.toml:152` | `DEFAULT_TIMEOUT`                                 | config | `numeric_bounds.code_host_request_timeout`                                           |
| `config/signalboxd.example.toml:154` | `MAX_JOB_LOG_BYTES`                               | config | `numeric_bounds.max_job_log_bytes`                                                   |
| `config/signalboxd.example.toml:156` | `MAX_STACK_COMPARISONS_IN_FLIGHT`                 | config | `numeric_bounds.max_stack_comparisons_in_flight`                                     |
| `config/signalboxd.example.toml:158` | `MAX_RESULT_TEXT_BYTES`                           | config | `numeric_bounds.max_code_host_result_text_bytes`                                     |
| `config/signalboxd.example.toml:160` | `MAX_RESULT_ITEMS`                                | config | `numeric_bounds.max_code_host_result_items`                                          |
| `config/signalboxd.example.toml:162` | `MAX_REPOSITORY_FILE_CONTENT_BYTES`               | config | `numeric_bounds.max_repository_file_content_bytes`                                   |
| `config/signalboxd.example.toml`     | `SESSION_ADMISSION_DEADLINE`                      | config | `numeric_bounds.session_admission_deadline`                                          |
| `config/signalboxd.example.toml`     | `SESSION_ACTIVE_STALL_DEADLINE`                   | config | `numeric_bounds.session_active_stall_deadline`                                       |
| `config/signalboxd.example.toml`     | `SESSION_WAITING_DEADLINE`                        | config | `numeric_bounds.session_waiting_deadline`                                            |
| `config/signalboxd.example.toml`     | `SESSION_LIFECYCLE_METRIC_SCAN_INTERVAL`          | config | `numeric_bounds.session_lifecycle_metric_scan_interval`                              |

## Representation facts

| Source                                                         | Constant                                        | Tier        | Why it is not a bound                                   |
| -------------------------------------------------------------- | ----------------------------------------------- | ----------- | ------------------------------------------------------- |
| `crates/process-protocol/src/lib.rs:114`                       | `MAX_DOLLAR_AMOUNT_BYTES`                       | not-a-bound | Longest canonical `rust_decimal` spelling.              |
| `crates/process-protocol/src/lib.rs:1018`                      | `MAX_DECIMAL_COEFFICIENT`                       | not-a-bound | Fixed `rust_decimal` coefficient representation.        |
| `apps/client/src/arguments.rs:2649`                            | `MAXIMUM_REVIEW_CONFIDENCE_BASIS_POINTS`        | not-a-bound | Fixed full-scale basis-point representation.            |
| `crates/tools-code-host/src/code_host/github.rs:51`            | `MAX_JSON_ESCAPE_BYTES_PER_SOURCE_BYTE`         | not-a-bound | Fixed maximum JSON escape expansion.                    |
| `crates/tools-code-host/src/code_host/github.rs:57`            | `MAX_REPOSITORY_CONTENTS_PATH_FIELDS_PER_ENTRY` | not-a-bound | Fixed provider response field count.                    |
| `crates/tools-code-host/src/code_host/github.rs:75`            | `MAX_COMMIT_SHA_RESPONSE_BYTES`                 | not-a-bound | A fixed-width commit SHA plus one optional newline.     |
| `crates/tools-code-host/src/code_host/github.rs:84`            | `MAX_CHANGED_FILE_PAGES`                        | not-a-bound | Fixed provider exposure divided by its fixed page size. |
| `crates/tools-code-host/src/code_host/repository_result.rs:21` | `MAX_OBSERVED_DIRECTORY_ENTRIES`                | not-a-bound | Fixed provider contents-endpoint exposure.              |
| `crates/tools-code-host/src/code_host/repository_result.rs:23` | `MAX_UTF8_BOUNDARY_DISCARD_BYTES`               | not-a-bound | Fixed maximum UTF-8 continuation width.                 |
| `crates/persistence/src/lifecycle_metrics.rs:26`               | `PARTS_PER_MILLION`                             | not-a-bound | Fixed-point scale the rates are reported in.            |

## Test fixtures

| Source                                                   | Constant                            | Tier | Fixture role                                                |
| -------------------------------------------------------- | ----------------------------------- | ---- | ----------------------------------------------------------- |
| `apps/signalboxd/tests/process_protocol_runtime.rs:2250` | `FLEET_OCCUPANCY_BOUND`             | test | Exercises production recovery promptly.                     |
| `apps/signalboxd/tests/process_protocol_runtime.rs:2252` | `FLEET_ASSERTION_BOUND`             | test | Keeps each fault probe within one CI minute.                |
| `apps/signalboxd/tests/process_protocol_runtime.rs:2254` | `FLEET_SETUP_BOUND`                 | test | Admits a full contended fleet within two CI minutes.        |
| `crates/tools-git/src/tests/layout.rs:42`                | `WIDE_ADMINISTRATIVE_SIBLING_COUNT` | test | Exceeds the dogfood supervisor's former descriptor ceiling. |
