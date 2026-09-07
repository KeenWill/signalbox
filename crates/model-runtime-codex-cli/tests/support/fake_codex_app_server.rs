//! Scripted offline Codex executable used only by this crate's integration
//! tests.

#![allow(
    clippy::expect_used,
    reason = "this standalone process-test peer reports fixture failures with assertion panics"
)]

use serde_json::{Value, json};

use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

mod fixtures;

// Arbitrary dropped reasoning payloads for the process fixtures.
const REASONING_TEXT: &str = "considering";
const PENDING_PROGRESS_TEXT: &str = "harmless";

static SUMMARY: Mutex<Option<String>> = Mutex::new(None);
static THREAD: OnceLock<String> = OnceLock::new();
static ERROR_TAG: OnceLock<String> = OnceLock::new();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    record_spawn()?;
    validate_argv()?;
    if Path::new(fixtures::EARLY_STDIN_HELD_EXIT_MARKER).exists() {
        // A descendant inherits the stdin read end and never reads it, so the
        // adapter's oversized upload stays blocked (no EPIPE) while the
        // leader's definitive nonzero exit becomes waitable.
        eprintln!("authentication failed");
        let descendant = std::process::Command::new("sleep")
            .arg("60")
            .stdin(Stdio::inherit())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        std::fs::write(
            "fake-codex-stdin-held-group",
            format!(
                "process_group={}\ndescendant={}\n",
                std::process::id(),
                descendant.id()
            ),
        )?;
        std::process::exit(7);
    }
    if Path::new("fake-codex-block-stdin").exists() {
        std::fs::write("fake-codex-block-stdin-ready", "ready\n")?;
        std::thread::sleep(Duration::from_secs(60));
    }
    let selected =
        std::env::args().find_map(|arg| arg.strip_prefix("--fixture=").map(str::to_owned));
    let initialize = read_frame()?;
    assert_eq!(initialize["method"], "initialize");
    emit_value(json!({"id":1,"result":{}}));
    assert_eq!(read_frame()?["method"], "initialized");
    let read_limits = read_frame()?;
    assert_eq!(read_limits["method"], "account/rateLimits/read");
    let rate_limits = match selected.as_deref() {
        Some(
            "capacity_read"
            | "capacity_sparse"
            | "capacity_failure"
            | "capacity_read_late"
            | "capacity_read_after_completion"
            | "capacity_read_stale"
            | "capacity_read_stale_after_completion",
        ) => json!({
            "primary":{"usedPercent":27,"windowDurationMins":300,"resetsAt":1800000700},
            "secondary":{"usedPercent":61,"windowDurationMins":10080,"resetsAt":1800001400}
        }),
        _ => json!({}),
    };
    if selected.as_deref() == Some("capacity_read_error") {
        emit_value(
            json!({"id":read_limits["id"],"error":{"code":-32600,"message":"rate limits unavailable"}}),
        );
    } else if !matches!(
        selected.as_deref(),
        Some(
            "capacity_read_pending"
                | "capacity_read_late"
                | "capacity_read_after_completion"
                | "capacity_read_stale"
                | "capacity_read_stale_after_completion"
        )
    ) {
        emit_value(json!({"id":read_limits["id"],"result":{"rateLimits":rate_limits}}));
    }
    let thread = read_frame()?;
    assert_eq!(thread["method"], "thread/start");
    assert_eq!(thread["params"]["ephemeral"], true);
    assert_eq!(thread["params"]["sandbox"], "read-only");
    assert_eq!(thread["params"]["approvalPolicy"], "never");
    std::fs::write("fake-codex-thread", thread["params"].to_string())?;
    let thread_id = if selected.as_deref() == Some("credential_prefix_thread_id_before_text") {
        fixtures::CREDENTIAL_PREFIX_THREAD_ID
    } else {
        fixtures::THREAD_ID
    };
    THREAD
        .set(thread_id.into())
        .map_err(|_| "duplicate thread")?;
    let mut response = json!({"id":2,"result":{"thread":{"id":thread_id}}});
    if selected.as_deref() == Some("credential_split_across_thread_started_field") {
        response["result"]["diagnostic"] = json!("Authorization:");
    }
    emit_value(response);
    if Path::new(fixtures::EARLY_STDIN_EXIT_MARKER).exists() {
        eprintln!("Codex rejected stdin");

        emit(
            r#"{"method":"turn/started","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1"}}"#,
        );
        failed(fixtures::EARLY_STDIN_FAILURE);
    }
    if Path::new(fixtures::EARLY_STDIN_COMPLETION_MARKER).exists() {
        emit(
            r#"{"method":"turn/started","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1"}}"#,
        );
        envelope(&format!(
            r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
            fixtures::BUFFERED_ANSWER
        ));
        completed();
        return Ok(());
    }
    let turn = read_frame()?;
    assert_eq!(turn["method"], "turn/start");
    assert_eq!(turn["params"]["threadId"], thread_id);
    std::fs::write("fake-codex-turn", turn["params"].to_string())?;
    let prompt = turn["params"]["input"][0]["text"]
        .as_str()
        .ok_or("missing prompt")?;
    std::fs::write("fake-codex-prompt", prompt)?;
    let scenario = selected.unwrap_or(scenario(prompt)?);
    let output_schema = turn["params"]["outputSchema"].clone();
    emit_value(
        json!({"id":3,"result":{"turn":{"id":"turn-offline-1","status":"inProgress","items":[],"error":null}}}),
    );
    ERROR_TAG
        .set(
            match scenario.as_str() {
                "error_credential"
                | "credential_precedence"
                | "error_then_contradictory_turn_failed" => "unauthorized",
                "error_permission" => "cyberPolicy",
                "error_invalid_request" | "error_target_not_found" => "badRequest",
                "error_request_too_large" => "contextWindowExceeded",
                "error_rate_limited" | "rate_snapshot_past" => "rateLimitExceeded",
                "error_quota_exhausted"
                | "quota_snapshot_past"
                | "error_then_turn_failed"
                | "error_without_turn_failed"
                | "error_then_turn_completed" => "usageLimitExceeded",
                "error_overloaded" => "serverOverloaded",
                "error_provider_internal" => "internalServerError",
                _ => scenario
                    .strip_prefix("typed:")
                    .unwrap_or(if scenario.starts_with("proof_") {
                        "rateLimitExceeded"
                    } else {
                        "other"
                    }),
            }
            .into(),
        )
        .map_err(|_| "duplicate error tag")?;
    if let Some(violation) = fixtures::strict_schema_violation(&output_schema) {
        unrecoverable(&format!(
            "unexpected status 400 Bad Request: {{\"error\":{{\"type\":\"invalid_request_error\",\"code\":\"invalid_json_schema\",\"message\":\"{violation}\"}}}}"
        ));
    }
    match scenario.as_str() {
        "capacity_read"
        | "capacity_read_error"
        | "capacity_sparse"
        | "capacity_read_pending"
        | "capacity_read_late"
        | "capacity_read_after_completion"
        | "capacity_read_stale"
        | "capacity_read_stale_after_completion" => {
            if matches!(
                scenario.as_str(),
                "capacity_read_stale" | "capacity_read_stale_after_completion"
            ) {
                emit_value(
                    json!({"method":"account/rateLimits/updated","params":{"rateLimits":{
                        "primary":{"usedPercent":96,"windowDurationMins":300,"resetsAt":1800001800},
                        "secondary":{"usedPercent":93,"windowDurationMins":10080,"resetsAt":1800002400}
                    }}}),
                );
            }
            if scenario == "capacity_read_stale" {
                emit_value(json!({"id":read_limits["id"],"result":{"rateLimits":rate_limits}}));
            }
            if scenario == "capacity_read_late" {
                emit_value(
                    json!({"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":null,"secondary":null}}}),
                );
                emit_value(json!({"id":read_limits["id"],"result":{"rateLimits":rate_limits}}));
            }
            if scenario == "capacity_sparse" {
                emit_value(
                    json!({"method":"account/rateLimits/updated","params":{"rateLimits":{
                        "primary":null,"secondary":{"usedPercent":88,"windowDurationMins":10080,"resetsAt":1800001600}
                    }}}),
                );
            }
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
            if matches!(
                scenario.as_str(),
                "capacity_read_after_completion" | "capacity_read_stale_after_completion"
            ) {
                emit_value(json!({"id":read_limits["id"],"result":{"rateLimits":rate_limits}}));
            }
        }
        "capacity_failure" => {
            emit_value(
                json!({"method":"account/rateLimits/updated","params":{"rateLimits":{
                    "primary":{"usedPercent":105,"windowDurationMins":300,"resetsAt":1800001700},"secondary":null
                }}}),
            );
            failed("terminal rejection");
        }
        "rate_snapshot_past" | "quota_snapshot_past" => {
            emit_value(
                json!({"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":{"usedPercent":100,"resetsAt":1}}}}),
            );
            failed("terminal rejection");
        }
        name if name.starts_with("typed:") => {
            failed("authentication failed rate limit quota exhausted")
        }
        "proof_retry" => {
            notify(
                "error",
                json!({"error":{"message":"transient","codexErrorInfo":"serverOverloaded"},"willRetry":true}),
            );
            failed("terminal rejection");
        }
        "proof_usage" => {
            notify(
                "thread/tokenUsage/updated",
                json!({"tokenUsage":{"total":{"inputTokens":1,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0,"totalTokens":1}}}),
            );
            failed("terminal rejection");
        }
        "proof_assistant" => {
            notify(
                "item/agentMessage/delta",
                json!({"itemId":"assistant","delta":"accepted output"}),
            );
            failed("terminal rejection");
        }
        "proof_summary" => {
            notify(
                "turn/completed",
                json!({"turn":{"id":"turn-offline-1","status":"failed","items":[{"type":"reasoning","id":"reasoning"}],"error":{"message":"terminal rejection","codexErrorInfo":"rateLimitExceeded"}}}),
            );
        }
        "interrupted" => {
            notify(
                "turn/completed",
                json!({"turn":{"id":"turn-offline-1","status":"interrupted","items":[],"error":null}}),
            );
        }
        "buffered_completed" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "terminal_summary_recovery" => {
            retain_summary_message(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "terminal_summary_split_credential" => {
            reasoning(
                "reason-summary-split",
                &fixtures::SENSITIVE_SPLIT_STREAM_TOKEN[..3],
            );
            retain_summary_message(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                &fixtures::SENSITIVE_SPLIT_STREAM_TOKEN[3..]
            ));
            completed();
        }
        "streamed_completed" => {
            reasoning("reason-1", REASONING_TEXT);
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::STREAMED_ANSWER
            ));
            completed();
        }
        "split_stream_credential_between_reasoning_items" => {
            reasoning(
                "reason-split-1",
                &fixtures::SENSITIVE_SPLIT_STREAM_TOKEN[..3],
            );
            reasoning(
                "reason-split-2",
                &fixtures::SENSITIVE_SPLIT_STREAM_TOKEN[3..],
            );
            envelope(r#"{"outcome":"completed","text":"safe","tool_calls":[]}"#);
            completed();
        }
        "split_stream_credential_before_final_text" => {
            reasoning(
                "reason-split-final",
                &fixtures::SENSITIVE_SPLIT_STREAM_TOKEN[..3],
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                &fixtures::SENSITIVE_SPLIT_STREAM_TOKEN[3..]
            ));
            completed();
        }
        "split_stream_authorization_before_final_text" => {
            reasoning("reason-split-authorization", "Authorization:");
            envelope(&format!(
                r#"{{"outcome":"completed","text":" {}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "split_stream_authorization_before_tool_arguments" => {
            reasoning("reason-split-tool-arguments", "Authorization:");
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"call-split-tool","name":"{}","arguments":"{}"}}]}}"#,
                fixtures::TOOL_NAME,
                json_escape(&format!(
                    r#"{{"city":" {}"}}"#,
                    fixtures::SENSITIVE_SPLIT_AUTHORIZATION
                ))
            ));
            completed();
        }
        "final_text_marker_before_tool_arguments" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"Authorization:","tool_calls":[{{"id":"call-final-text","name":"{}","arguments":"{}"}}]}}"#,
                fixtures::TOOL_NAME,
                json_escape(&format!(
                    r#"{{"value":" {}"}}"#,
                    fixtures::SENSITIVE_SPLIT_AUTHORIZATION
                ))
            ));
            completed();
        }
        "split_stream_authorization_before_tool_id" => {
            reasoning("reason-split-tool-id", "Authorization:");
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":" {}","name":"{}","arguments":"{{}}"}}]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION,
                fixtures::TOOL_NAME
            ));
            completed();
        }
        "split_stream_authorization_before_message_id" => {
            reasoning("reason-split-message-id", "Authorization:");
            agent_message(
                &format!(" {}", fixtures::SENSITIVE_SPLIT_AUTHORIZATION),
                r#"{"outcome":"completed","text":"safe","tool_calls":[]}"#,
            );
            completed();
        }
        "final_text_marker_before_message_id" => {
            agent_message(
                &format!(" {}", fixtures::SENSITIVE_SPLIT_AUTHORIZATION),
                r#"{"outcome":"completed","text":"Authorization:","tool_calls":[]}"#,
            );
            completed();
        }
        "credential_split_across_thread_started_field" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":" {}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "two_independent_sibling_markers" => {
            // Two independent object fields each end in a distinct credential
            // marker; a following value could complete either, so the single
            // dropped chain fails closed.
            emit(
                r#"{"method":"item/completed","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"two","type":"future_item","a_field":"api_","z_field":"refresh_tok"}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "streamed_empty_final_text_with_held_credential" => {
            // A streamed reasoning delta holds a complete credential token, so
            // `redact_terminal_failure_text("")` returns `[redacted]` for the
            // empty final text; with no text delta and no tools, the empty
            // completion must fail closed as unintelligible, not surface as a
            // contentless Completed.
            reasoning("reason-held", fixtures::SENSITIVE_SPLIT_STREAM_TOKEN);
            envelope(r#"{"outcome":"completed","text":"","tool_calls":[]}"#);
            completed();
        }
        "credential_split_across_sibling_object_fields" => {
            // A marker-bearing field sorts BEFORE a benign sibling, so a
            // document-order (serde key-sorted) concatenation would lose the
            // `api_` marker; the strongest-unit seeding must keep it.
            emit(
                r#"{"method":"item/completed","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"sib","type":"future_item","a_marker":"api_","z_benign":"notice"}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "credential_split_across_turn_started_field" => {
            // A drifted turn.started carries an additive credential marker.
            emit(
                r#"{"method":"turn/started","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","diagnostic":"Authorization:"}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":" {}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "credential_split_across_agent_message_then_failure" => {
            agent_message("msg-before-failure", "leaked Authorization:");
            failed(&format!(" {}", fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
        }
        "credential_split_across_superseded_agent_message" => {
            // An earlier agent message is superseded by a later one; its
            // trailing marker plus the final message's value must not
            // reconstruct across the discard.
            agent_message("msg-superseded", "leaked Authorization:");
            envelope(&format!(
                r#"{{"outcome":"completed","text":" {}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "credential_split_across_lifecycle_event" => {
            emit(
                r#"{"method":"item/started","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"life","type":"future_item","aggregated_output":"Authorization:"}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":" {}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "credential_split_across_unknown_event" => {
            // An additively-tolerated unknown top-level event carries the
            // marker; the final text supplies the value.
            emit(
                r#"{"method":"diagnostic_event","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","content":"Authorization:"}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":" {}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "credential_split_across_ordered_unsupported_leaves" => {
            // An unmodeled item's ordered array leaves jointly form the marker
            // `api_key=` that no single leaf shows.
            emit(
                r#"{"method":"item/completed","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"arr","type":"future_item","fields":["api","_key="]}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "message_id_prefixes_final_text" => {
            // The agent-message id ends in a credential-marker prefix and the
            // final text opens with its continuation, reconstructing across the
            // id and content fields of terminal evidence.
            agent_message(
                "api_",
                &format!(
                    r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                    fixtures::SENSITIVE_SPLIT_AUTHORIZATION
                ),
            );
            completed();
        }
        "credential_split_across_unsupported_item" => {
            unsupported_item("diag-marker", "api_");
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "credential_split_across_dropped_pending_and_error_separator" => {
            // Chronological provider text `api_key=<secret>`: a dropped error
            // item seeds `api_`, a streamed reasoning delta contributes `key`
            // (held only because it continues `api_`), a second dropped error
            // supplies `=`, and the value arrives in the final text.
            error_item("error-prefix", "api_");
            reasoning("reason-key", "key");
            error_item("error-separator", "=");
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "unsupported_item_marker_in_nonstandard_field" => {
            // The marker is in a non-`text`/`message` field of an unmodeled
            // item; every string leaf must still seed the lookbehind.
            emit(
                r#"{"method":"item/completed","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"diag","type":"diagnostic","content":"api_"}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "unsupported_item_marker_beside_benign_field" => {
            // An unmodeled item carries a marker in one field and benign text
            // in another; the benign field must not erase the marker.
            emit(
                r#"{"method":"item/completed","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"diag","type":"diagnostic","message":"diagnostic notice","text":"api_"}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "held_credential_prefix_then_unrelated_error_item" => {
            // The reasoning delta is itself an incomplete credential prefix
            // (`api_`), held in the lookbehind; an unrelated error item
            // follows, and the value arrives in the final text. The dropped
            // error bytes never reach output, so `api_` stays adjacent to
            // `key=<secret>` there and the value must be suppressed.
            reasoning("reason-prefix", "api_");
            error_item("error-unrelated", "diagnostic notice");
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "credential_reassembled_across_held_reasoning_and_error_item" => {
            // The held reasoning bytes (`Auth`, an unrelated unsafe suffix)
            // are chronologically before the dropped error marker (`api_`)
            // and the final-text value (`key=<secret>`); only the dropped
            // marker plus the value form the credential, so the held bytes
            // must not be scanned between them.
            reasoning("reason-unrelated", "Auth");
            error_item("error-marker", "api_");
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "credential_split_across_error_item_after_held_reasoning" => {
            // A streamed reasoning delta ends in a bare marker word (held as
            // an unsafe suffix), an intervening error item supplies only the
            // separator, and the final text begins with the value: the held
            // `Authorization`, the dropped `:`, and the value must rejoin in
            // chronological order.
            reasoning("reason-held-marker", "Authorization");
            error_item("error-separator", ":");
            envelope(&format!(
                r#"{{"outcome":"completed","text":" {}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "credential_split_across_error_item" => {
            error_item("error-marker", "Authorization:");
            envelope(&format!(
                r#"{{"outcome":"completed","text":" {}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "credential_prefix_thread_id_before_text" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_THREAD_CONTINUATION
            ));
            completed();
        }
        "split_stream_authorization_before_tool_name" => {
            reasoning("reason-split-tool-name", "Authorization:");
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"call-name","name":" {}","arguments":"{{}}"}}]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "reasoning_then_malformed_usage" => {
            reasoning("reason-held-marker", "Authorization:");
            usage_and_complete(&format!(
                r#"{{"inputTokens":" {}","cachedInputTokens":2,"cacheWriteInputTokens":1,"outputTokens":7,"reasoningOutputTokens":3}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
        }
        "dropped_marker_then_malformed_usage" => {
            // A dropped error item seeds a marker prefix, then a known event
            // fails shape decoding with the continuation quoted inside serde's
            // own prose — which no joined-form scan can rejoin across.
            error_item("err-marker", "api_");
            usage_and_complete(&format!(
                r#"{{"inputTokens":"key={}","cachedInputTokens":2,"cacheWriteInputTokens":1,"outputTokens":7}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
        }
        "split_stream_authorization_before_failure" => {
            reasoning("reason-split-authorization-failed", "Authorization:");
            unrecoverable(&format!(" {}", fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
        }
        "last_agent_message" => {
            agent_message("message-intermediate", "not a response envelope");
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "malformed_last_agent_message" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            agent_message("message-last", "not a response envelope");
            completed();
        }
        "credential_envelope_error" => {
            envelope(&format!(
                r#"{{"outcome":"{}","text":"","tool_calls":[]}}"#,
                fixtures::SENSITIVE_ENVELOPE_TOKEN
            ));
            completed();
        }
        "deep_agent_message" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"call-deep","name":"{}","arguments":{}}}]}}"#,
                fixtures::TOOL_NAME,
                deeply_nested_arguments()
            ));
            completed();
        }
        "tool_call" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"call-offline-1","name":"{}","arguments":"{}"}}]}}"#,
                fixtures::TOOL_NAME,
                json_escape(fixtures::TOOL_ARGUMENTS)
            ));
            completed();
        }
        "tool_call_bad_arguments" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"call-offline-bad","name":"{}","arguments":"{}"}}]}}"#,
                fixtures::TOOL_NAME,
                fixtures::MALFORMED_TOOL_ARGUMENTS
            ));
            completed();
        }
        "tool_call_non_object_arguments" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"call-offline-non-object","name":"{}","arguments":"{}"}}]}}"#,
                fixtures::TOOL_NAME,
                fixtures::NON_OBJECT_TOOL_ARGUMENTS
            ));
            completed();
        }
        "tool_call_deep_arguments" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"call-offline-deep","name":"{}","arguments":"{}"}}]}}"#,
                fixtures::TOOL_NAME,
                json_escape(&deeply_nested_arguments())
            ));
            completed();
        }
        "structured_output" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"structured-offline-1","name":"verdict","arguments":"{}"}}]}}"#,
                json_escape(&format!(
                    r#"{{ "accepted" : {} }}"#,
                    fixtures::STRUCTURED_ACCEPTED
                ))
            ));
            completed();
        }
        "structured_output_missing" => {
            envelope(r#"{"outcome":"completed","text":"","tool_calls":[]}"#);
            completed();
        }
        "structured_output_multiple" => {
            let arguments = json_escape(&format!(
                r#"{{ "accepted" : {} }}"#,
                fixtures::STRUCTURED_ACCEPTED
            ));
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"structured-offline-1","name":"verdict","arguments":"{arguments}"}},{{"id":"structured-offline-2","name":"verdict","arguments":"{arguments}"}}]}}"#
            ));
            completed();
        }
        "named_choice_extra_tool" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"named-offline-1","name":"{}","arguments":"{}"}},{{"id":"named-offline-2","name":"{}","arguments":"{{}}"}}]}}"#,
                fixtures::TOOL_NAME,
                json_escape(fixtures::TOOL_ARGUMENTS),
                fixtures::OTHER_TOOL_NAME
            ));
            completed();
        }
        "bare_credential_text" => {
            let text = json_escape(&format!(
                r#"  "client_secret":"{}""#,
                fixtures::SENSITIVE_COMPOSITE_SECRET
            ));
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{text}","tool_calls":[]}}"#
            ));
            completed();
        }
        "structured_credential_value" => {
            let text = json_escape(&format!(
                r#"provider detail: {{"credential":{{"value":"{}"}}}}"#,
                fixtures::SENSITIVE_STRUCTURED_SECRET
            ));
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{text}","tool_calls":[]}}"#
            ));
            completed();
        }
        "split_stream_structured_credential" => {
            reasoning("reason-structured-1", r#"{"credential":{"value":"#);
            reasoning(
                "reason-structured-2",
                &format!(r#""{}"}}}}"#, fixtures::SENSITIVE_STRUCTURED_SECRET),
            );
            envelope(r#"{"outcome":"completed","text":"safe","tool_calls":[]}"#);
            completed();
        }
        "agent_message_additive_field_marker" => {
            // A known agent-message item carrying an additively tolerated
            // sibling serde discards; its marker must still govern the
            // envelope text the same item retains.
            emit(&format!(
                r#"{{"method":"item/completed","params":{{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{{"id":"message-offline-1","type":"agentMessage","diagnostic":"api_","text":"{}"}}}}}}"#,
                json_escape(&format!(
                    r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                    fixtures::SENSITIVE_SPLIT_AUTHORIZATION
                ))
            ));
            completed();
        }
        "turn_failed_additive_field_marker" => {
            // An additive field on the failure event holds the marker its own
            // interpreted message completes.
            emit(&format!(
                r#"{{"method":"turn/completed","params":{{"threadId":"thread-offline-1","turnId":"turn-offline-1","turn":{{"id":"turn-offline-1","status":"failed","items":[],"diagnostic":"api_","error":{{"message":"key={}"}}}}}}}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
        }
        "superseded_message_marker_before_clean_id" => {
            // The superseded message ends in a live marker while its id is
            // clean; folding the id after the text would resolve the chain and
            // release the value the final text completes.
            agent_message("superseded-done.", "api_");
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "unsupported_item_object_inside_an_array" => {
            // The marker and a benign sibling are fields of an object nested
            // inside an array; joining them into one wire-adjacent unit would
            // erase the marker.
            emit(
                r#"{"method":"item/completed","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"diag","type":"diagnostic","entries":[{"a_marker":"api_","z_benign":"done."}]}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "unknown_event_metadata_marker" => {
            // An unknown event interprets nothing — its `type` matched no known
            // event and its `id` is never validated or emitted — so every field
            // is dropped provider content that must still govern what follows.
            emit(
                r#"{"method":"future","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","id":"api_"}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "lifecycle_item_id_marker" => {
            // A bare lifecycle event is dropped whole after its identity is
            // validated as nonempty. The id ends in a credential-marker prefix
            // and the final text opens with the continuation.
            emit(
                r#"{"method":"item/started","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"trace-api_","type":"future_item"}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "lifecycle_item_type_marker" => {
            // The same boundary one field over: the lifecycle event's item
            // `type` matched no arm of the adapter's, so it is provider text
            // ending in the marker prefix.
            emit(
                r#"{"method":"item/started","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"item_7","type":"future_api_"}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "unsupported_item_type_marker" => {
            // An unmodeled item's `type` is provider-chosen (it selected the
            // catch-all arm rather than one of the adapter's literals) and the
            // whole item is dropped.
            emit(
                r#"{"method":"item/completed","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"item_7","type":"future_api_"}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "reasoning_item_id_marker" => {
            emit(
                r#"{"method":"item/completed","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"trace-api_","type":"reasoning","text":"key="}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "error_item_id_marker" => {
            // The same shape on the dropped error item, whose message is
            // interpreted while its id is not.
            emit(
                r#"{"method":"item/completed","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"trace-api_","type":"error","message":"key="}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "benign_item_identity_before_answer" => {
            // Ordinary identity metadata around an unmodeled item: an id
            // ending in a digit and a real Codex item type. `todo_list` ends
            // in bytes the lookbehind holds conservatively (a name that could
            // still grow into `token`), so this is the control that folding
            // the identity does not turn routine metadata into suppression —
            // the answer must still reach the caller byte-verbatim.
            emit(
                r#"{"method":"item/started","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"item_7","type":"todo_list"}}}"#,
            );
            emit(
                r#"{"method":"item/completed","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"item_7","type":"todo_list","text":"update the plan"}}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "duplicate_unknown_event_member" => {
            // Repeated members are ambiguous provider input even on an
            // otherwise additively tolerated unknown event. The adapter must
            // reject the event before serde's last-value-wins projection can
            // discard either occurrence.
            emit(
                r#"{"method":"future","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","note":"first","note":"second"}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "nested_duplicate_unknown_event_member" => {
            emit(
                r#"{"method":"future","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","items":[{"note":"first","note":"second"}]}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "duplicate_response_envelope_member" => {
            envelope(r#"{"outcome":"completed","text":"first","text":"second","tool_calls":[]}"#);
            completed();
        }
        "textless_refusal" => {
            envelope(r#"{"outcome":"refused","text":"","tool_calls":[]}"#);
            completed();
        }
        "structured_refused" => {
            envelope(&format!(
                r#"{{"outcome":"refused","text":"{}","tool_calls":[]}}"#,
                fixtures::REFUSAL_TEXT
            ));
            completed();
        }
        "refused" => {
            envelope(&format!(
                r#"{{"outcome":"refused","text":"{}","tool_calls":[]}}"#,
                fixtures::REFUSAL_TEXT
            ));
            completed();
        }
        "credential_precedence" => {
            envelope(&format!(
                r#"{{"outcome":"refused","text":"{}","tool_calls":[]}}"#,
                fixtures::REFUSAL_TEXT
            ));
            failed("authentication failed after refusal");
        }
        "error_permission" => failed("permission denied"),
        "error_invalid_request" => failed("invalid request"),
        "error_target_not_found" => failed("model not found"),
        "error_request_too_large" => failed("request too large"),
        "error_rate_limited" => failed("rate limit exceeded"),
        "error_quota_exhausted" => failed("insufficient_quota"),
        "error_overloaded" => failed("provider overloaded"),
        "error_provider_internal" => failed("internal server error"),
        "error_unrecognized" => failed("future failure shape"),
        "error_then_turn_failed" => unrecoverable(fixtures::STREAM_ERROR_MESSAGE),
        // A stream-level error with no lifecycle echo at all: the process just
        // ends. This is the shape the substitution proof must refuse.
        "error_without_turn_failed" => {
            emit_error(fixtures::STREAM_ERROR_MESSAGE);
            std::process::exit(1);
        }
        "error_then_turn_completed" => {
            emit_error(fixtures::STREAM_ERROR_MESSAGE);
            completed();
        }
        "error_then_contradictory_turn_failed" => {
            emit_error(fixtures::STREAM_ERROR_MESSAGE);
            failed("authentication failed instead of the stream error");
        }
        "no_terminal" => {
            envelope(r#"{"outcome":"completed","text":"not terminal","tool_calls":[]}"#);
        }
        "malformed_event" => emit("{not-json"),
        "reasoning_then_malformed_event" => {
            reasoning("reason-before-malformed", PENDING_PROGRESS_TEXT);
            emit("{not-json");
        }
        "malformed_known_lifecycle" => {
            emit(
                r#"{"method":"item/started","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1"}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "empty_completed_item_identity" => {
            agent_message(
                "",
                &format!(
                    r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                    fixtures::BUFFERED_ANSWER
                ),
            );
            completed();
        }
        "redaction" => {
            let text = format!(
                r#"Bearer {} and {{"client_secret":"{}"}}"#,
                fixtures::SENSITIVE_OUTPUT_TOKEN,
                fixtures::SENSITIVE_COMPOSITE_SECRET
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[{{"id":"call-redaction","name":"{}","arguments":"{}"}}]}}"#,
                json_escape(&text),
                fixtures::TOOL_NAME,
                json_escape(&format!(
                    r#"{{"access_token":"{}","city":"Oslo"}}"#,
                    fixtures::SENSITIVE_REFRESH_TOKEN
                ))
            ));
            completed();
        }
        "sensitive_tool_ids" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"","tool_calls":[{{"id":"{}","name":"{}","arguments":"{{}}"}},{{"id":"{}","name":"{}","arguments":"{{}}"}}]}}"#,
                fixtures::SENSITIVE_TOOL_ID_ONE,
                fixtures::TOOL_NAME,
                fixtures::SENSITIVE_TOOL_ID_TWO,
                fixtures::TOOL_NAME
            ));
            completed();
        }
        "usage_without_cache" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed_without_cache();
        }
        "usage_sparse_updates" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            notify(
                "thread/tokenUsage/updated",
                json!({"tokenUsage":{"total":{"inputTokens":11,"outputTokens":3,"cacheWriteInputTokens":7,"cachedInputTokens":5}}}),
            );
            notify(
                "thread/tokenUsage/updated",
                json!({"tokenUsage":{"total":{"outputTokens":9}}}),
            );
            usage_and_complete(r#"{"inputTokens":13,"totalTokens":22}"#);
        }
        "usage_partial_axes" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            usage_and_complete(&format!(
                r#"{{"cachedInputTokens":{},"outputTokens":{},"totalTokens":19}}"#,
                fixtures::CACHE_READ_INPUT_TOKENS,
                fixtures::OUTPUT_TOKENS
            ));
        }
        "usage_total_only" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            usage_and_complete(r#"{"totalTokens":19}"#);
        }
        "completion_before_cancellation" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
            // Settle after the terminal marker so the adapter has consumed it
            // before the readiness file lets the marker-watching cancellation
            // fire; otherwise a loaded runner can kill the process group before
            // the terminal line is read, and the exchange races to
            // StreamEndedWithoutTerminalMarker instead of the completion the
            // work-first rule guarantees.
            std::thread::sleep(Duration::from_secs(1));
            std::fs::write("fake-codex-completion-ready", "ready\n")?;
            std::thread::sleep(Duration::from_secs(60));
        }
        "inherited_stderr" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
            let descendant = std::process::Command::new("sh")
                .arg("-c")
                .arg("sleep 60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?;
            record_process_group("fake-codex-inherited-stderr-process-group", descendant.id())?;
        }
        "completed_with_detached_descendant" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
            let descendant = std::process::Command::new("sh")
                .arg("-c")
                .arg("sleep 60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            record_process_group(
                "fake-codex-detached-descendant-process-group",
                descendant.id(),
            )?;
        }
        "interrupt_with_descendant" => {
            let descendant = std::process::Command::new("sh")
                .arg("-c")
                .arg("trap '' INT; sleep 60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            record_process_group(
                "fake-codex-interrupt-descendant-process-group",
                descendant.id(),
            )?;
            std::thread::sleep(Duration::from_secs(60));
        }
        "filtered_environment" => {
            if std::env::var_os("PWD").is_some() || std::env::var_os("PATH").is_none() {
                failed("subprocess environment was not filtered");
            } else {
                envelope(&format!(
                    r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                    fixtures::BUFFERED_ANSWER
                ));
                completed();
            }
        }
        "selected_credential_home" => {
            let home = std::env::var_os("CODEX_HOME")
                .ok_or("selected credential home was not delivered")?;
            let target = std::fs::read_link(Path::new(&home).join("auth.json"))?;
            std::fs::write(
                "fake-codex-selected-home",
                target
                    .parent()
                    .ok_or("auth parent")?
                    .as_os_str()
                    .as_encoded_bytes(),
            )?;
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "stderr_credential_continuation" => {
            reasoning("reason-stderr-continuation", "Authoriz");
            eprintln!("ation: {}", fixtures::SENSITIVE_STDERR_CONTINUATION);
            std::process::exit(7);
        }
        "stderr_redaction" => {
            eprintln!(
                "authentication failed API_KEY=\"{}\"",
                fixtures::SENSITIVE_STDERR_TOKEN
            );
            std::process::exit(7);
        }
        "killed_process" => std::process::abort(),
        "hang" => std::thread::sleep(Duration::from_secs(60)),
        "busy_stdout" => {
            std::fs::write("fake-codex-busy-stdout", "ready\n")?;
            loop {
                emit(
                    r#"{"method":"item/started","params":{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{"id":"busy-progress","type":"future_item"}}}"#,
                );
            }
        }
        _ => failed("invalid request: unknown offline fixture"),
    }
    Ok(())
}

fn record_process_group(path: &str, descendant: u32) -> std::io::Result<()> {
    std::fs::write(
        path,
        format!(
            "process_group={}\ndescendant={descendant}\n",
            std::process::id()
        ),
    )
}

fn record_spawn() -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("fake-codex-spawns")?;
    writeln!(file, "spawn")
}

fn validate_argv() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().collect();
    std::fs::write("fake-codex-argv", arguments.join("\n"))?;
    let server = arguments
        .iter()
        .position(|arg| arg == "app-server")
        .ok_or("missing app-server")?;
    assert!(arguments[..server].iter().any(|arg| arg == "--disable"));
    for flag in [
        "--stdio",
        "--strict-config",
        "--ignore-user-config",
        "--ignore-rules",
    ] {
        assert!(arguments[server..].iter().any(|arg| arg == flag));
    }
    let home = std::env::var_os("CODEX_HOME").ok_or("missing operation home")?;
    assert_eq!(std::fs::read(Path::new(&home).join("config.toml"))?, b"");
    Ok(())
}

fn read_frame() -> Result<Value, Box<dyn std::error::Error>> {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

fn emit_value(value: Value) {
    println!("{value}");
}
fn notify(method: &str, mut params: Value) {
    params["threadId"] = json!(
        THREAD
            .get()
            .map(String::as_str)
            .unwrap_or(fixtures::THREAD_ID)
    );
    params["turnId"] = json!("turn-offline-1");
    emit_value(json!({"method":method,"params":params}));
}

fn scenario(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    let request = prompt
        .rsplit_once("\n\n")
        .map(|(_, request)| request.trim())
        .ok_or("missing rendered request")?;
    let value: serde_json::Value = serde_json::from_str(request)?;
    value["messages"][0]["parts"][0]["text"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "missing fixture scenario".into())
}

fn envelope(value: &str) {
    agent_message("message-offline-1", value);
}

fn agent_message(id: &str, value: &str) {
    let escaped = json_escape(value);
    emit(&format!(
        r#"{{"method":"item/completed","params":{{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{{"id":"{id}","type":"agentMessage","text":"{escaped}"}}}}}}"#
    ));
}

fn retain_summary_message(value: &str) {
    *SUMMARY.lock().expect("summary fixture lock") = Some(value.to_owned());
}

fn error_item(id: &str, message: &str) {
    emit(&format!(
        r#"{{"method":"item/completed","params":{{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{{"id":"{id}","type":"error","message":"{}"}}}}}}"#,
        json_escape(message)
    ));
}

fn unsupported_item(id: &str, text: &str) {
    // An item type the adapter does not model; its `text` is dropped from the
    // output but must still seed the redaction lookbehind.
    emit(&format!(
        r#"{{"method":"item/completed","params":{{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{{"id":"{id}","type":"diagnostic","text":"{}"}}}}}}"#,
        json_escape(text)
    ));
}

fn reasoning(id: &str, text: &str) {
    emit(&format!(
        r#"{{"method":"item/completed","params":{{"threadId":"thread-offline-1","turnId":"turn-offline-1","item":{{"id":"{id}","type":"reasoning","text":"{}"}}}}}}"#,
        json_escape(text)
    ));
}

fn completed() {
    usage_and_complete(&json!({"inputTokens":fixtures::INPUT_TOKENS,"cachedInputTokens":fixtures::CACHE_READ_INPUT_TOKENS,"cacheWriteInputTokens":fixtures::CACHE_CREATION_INPUT_TOKENS,"outputTokens":fixtures::OUTPUT_TOKENS,"reasoningOutputTokens":3,"totalTokens":18}).to_string());
}

fn completed_without_cache() {
    usage_and_complete(&json!({"inputTokens":fixtures::INPUT_TOKENS,"cachedInputTokens":0,"outputTokens":fixtures::OUTPUT_TOKENS,"reasoningOutputTokens":3,"totalTokens":18}).to_string());
}

fn usage_and_complete(usage: &str) {
    notify(
        "thread/tokenUsage/updated",
        json!({"tokenUsage":{"total":serde_json::from_str::<Value>(usage).expect("usage fixture is JSON")}}),
    );
    let items: Vec<Value> = SUMMARY
        .lock()
        .expect("summary fixture lock")
        .take()
        .into_iter()
        .map(|text| json!({"type":"agentMessage","id":"message-offline-1","text":text}))
        .collect();
    notify(
        "turn/completed",
        json!({"turn":{"id":"turn-offline-1","status":"completed","items":items,"error":null}}),
    );
}

fn deeply_nested_arguments() -> String {
    let mut value = "{}".to_string();
    for _ in 0..130 {
        value = format!(r#"{{"nested":{value}}}"#);
    }
    value
}

fn error_info() -> Value {
    let tag = ERROR_TAG
        .get()
        .map(String::as_str)
        .unwrap_or("contextWindowExceeded");
    serde_json::from_str(tag).unwrap_or_else(|_| json!(tag))
}

fn failed(message: &str) -> ! {
    notify(
        "turn/completed",
        json!({"turn":{"id":"turn-offline-1","status":"failed","items":[],"error":{"message":message,"codexErrorInfo":error_info()}}}),
    );
    std::process::exit(1);
}

fn unrecoverable(message: &str) -> ! {
    emit_error(message);
    failed(message)
}

fn emit_error(message: &str) {
    notify(
        "error",
        json!({"error":{"message":message,"codexErrorInfo":ERROR_TAG.get().map(String::as_str).unwrap_or("other")},"willRetry":false}),
    );
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn emit(line: &str) {
    let line = line.replace(
        fixtures::THREAD_ID,
        THREAD
            .get()
            .map(String::as_str)
            .unwrap_or(fixtures::THREAD_ID),
    );
    println!("{line}");
}
