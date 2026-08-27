//! Scripted offline Codex executable used only by this crate's integration
//! tests.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

mod fixtures;

static OUTPUT_LAST_MESSAGE_PATH: OnceLock<PathBuf> = OnceLock::new();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    record_spawn()?;
    let output_schema = validate_argv()?;
    if Path::new(fixtures::EARLY_STDIN_EXIT_MARKER).exists() {
        eprintln!("Codex rejected stdin");
        emit(&format!(
            r#"{{"type":"thread.started","thread_id":"{}"}}"#,
            fixtures::THREAD_ID
        ));
        emit(r#"{"type":"turn.started"}"#);
        failed(fixtures::EARLY_STDIN_FAILURE);
    }
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
    if Path::new(fixtures::EARLY_STDIN_COMPLETION_MARKER).exists() {
        emit(&format!(
            r#"{{"type":"thread.started","thread_id":"{}"}}"#,
            fixtures::THREAD_ID
        ));
        emit(r#"{"type":"turn.started"}"#);
        envelope(&format!(
            r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
            fixtures::BUFFERED_ANSWER
        ));
        completed();
        return Ok(());
    }
    if Path::new("fake-codex-block-stdin").exists() {
        std::fs::write("fake-codex-block-stdin-ready", "ready\n")?;
        std::thread::sleep(Duration::from_secs(60));
    }
    let mut prompt = String::new();
    std::io::stdin().read_to_string(&mut prompt)?;
    std::fs::write("fake-codex-prompt", &prompt)?;

    let scenario = scenario(&prompt)?;
    // A drifted or hostile CLI can choose its thread id; this scenario's id
    // ends in a credential-marker prefix whose continuation the final text
    // carries.
    let thread_id = if scenario == "credential_prefix_thread_id_before_text" {
        fixtures::CREDENTIAL_PREFIX_THREAD_ID
    } else {
        fixtures::THREAD_ID
    };
    if scenario == "credential_split_across_thread_started_field" {
        // A drifted thread.started carries an additive credential marker beyond
        // the thread id.
        emit(&format!(
            r#"{{"type":"thread.started","thread_id":"{thread_id}","diagnostic":"Authorization:"}}"#
        ));
    } else {
        emit(&format!(
            r#"{{"type":"thread.started","thread_id":"{thread_id}"}}"#
        ));
    }
    emit(r#"{"type":"turn.started"}"#);
    if let Some(violation) = fixtures::strict_schema_violation(&output_schema) {
        // The live API rejects a non-strict output schema before producing
        // any model output, and the pinned CLI reports that rejection as a
        // stream-level `error` event followed by its `turn.failed` lifecycle
        // echo and a nonzero exit — the sequence the gated compatibility
        // smoke observed for `invalid_json_schema`. Enforcing the same rules
        // here keeps the offline corpus from accepting a schema the live API
        // refuses.
        unrecoverable(&format!(
            "unexpected status 400 Bad Request: {{\"error\":{{\"type\":\"invalid_request_error\",\"code\":\"invalid_json_schema\",\"message\":\"{violation}\"}}}}"
        ));
    }
    match scenario.as_str() {
        "buffered_completed" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "output_last_message_recovery" => {
            retain_last_message(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "output_last_message_split_credential" => {
            reasoning(
                "reason-output-file-split",
                &fixtures::SENSITIVE_SPLIT_STREAM_TOKEN[..3],
            );
            retain_last_message(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                &fixtures::SENSITIVE_SPLIT_STREAM_TOKEN[3..]
            ));
            completed();
        }
        "streamed_completed" => {
            reasoning("reason-1", fixtures::REASONING_TEXT);
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
            // thread.started (with the additive `diagnostic` marker above)
            // then the value in the final text.
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
                r#"{"type":"item.completed","item":{"id":"two","type":"future_item","a_field":"api_","z_field":"refresh_tok"}}"#,
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
                r#"{"type":"item.completed","item":{"id":"sib","type":"future_item","a_marker":"api_","z_benign":"notice"}}"#,
            );
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "credential_split_across_turn_started_field" => {
            // A drifted turn.started carries an additive credential marker.
            emit(r#"{"type":"turn.started","diagnostic":"Authorization:"}"#);
            envelope(&format!(
                r#"{{"outcome":"completed","text":" {}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "credential_split_across_agent_message_then_failure" => {
            // A retained agent message ends in a marker, then turn.failed's
            // message supplies the value.
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
            // An ignored item.started carries a credential marker in an
            // additive field; the final text supplies the value.
            emit(
                r#"{"type":"item.started","item":{"id":"life","type":"future_item","aggregated_output":"Authorization:"}}"#,
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
            emit(r#"{"type":"diagnostic_event","content":"Authorization:"}"#);
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
                r#"{"type":"item.completed","item":{"id":"arr","type":"future_item","fields":["api","_key="]}}"#,
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
                r#"{"type":"item.completed","item":{"id":"diag","type":"diagnostic","content":"api_"}}"#,
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
                r#"{"type":"item.completed","item":{"id":"diag","type":"diagnostic","message":"diagnostic notice","text":"api_"}}"#,
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
            emit(&format!(
                r#"{{"type":"turn.completed","usage":{{"input_tokens":" {}","cached_input_tokens":2,"cache_write_input_tokens":1,"output_tokens":7,"reasoning_output_tokens":3}}}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
        }
        "dropped_marker_then_malformed_usage" => {
            // A dropped error item seeds a marker prefix, then a known event
            // fails shape decoding with the continuation quoted inside serde's
            // own prose — which no joined-form scan can rejoin across.
            error_item("err-marker", "api_");
            emit(&format!(
                r#"{{"type":"turn.completed","usage":{{"input_tokens":"key={}","cached_input_tokens":2,"cache_write_input_tokens":1,"output_tokens":7}}}}"#,
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
                r#"{{"type":"item.completed","item":{{"id":"message-offline-1","type":"agent_message","diagnostic":"api_","text":"{}"}}}}"#,
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
                r#"{{"type":"turn.failed","diagnostic":"api_","error":{{"message":"key={}"}}}}"#,
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
                r#"{"type":"item.completed","item":{"id":"diag","type":"diagnostic","entries":[{"a_marker":"api_","z_benign":"done."}]}}"#,
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
            emit(r#"{"type":"future","id":"api_"}"#);
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
            emit(r#"{"type":"item.started","item":{"id":"trace-api_","type":"future_item"}}"#);
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
            emit(r#"{"type":"item.updated","item":{"id":"item_7","type":"future_api_"}}"#);
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
            emit(r#"{"type":"item.completed","item":{"id":"item_7","type":"future_api_"}}"#);
            envelope(&format!(
                r#"{{"outcome":"completed","text":"key={}","tool_calls":[]}}"#,
                fixtures::SENSITIVE_SPLIT_AUTHORIZATION
            ));
            completed();
        }
        "reasoning_item_id_marker" => {
            // A modeled reasoning item: its `type` is the adapter's own
            // literal and its text is interpreted, but the id is dropped.
            // Chronologically the provider wrote `api_` (id), `key=` (text)
            // and the value (final text).
            emit(
                r#"{"type":"item.completed","item":{"id":"trace-api_","type":"reasoning","text":"key="}}"#,
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
                r#"{"type":"item.completed","item":{"id":"trace-api_","type":"error","message":"key="}}"#,
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
            emit(r#"{"type":"item.started","item":{"id":"item_7","type":"todo_list"}}"#);
            emit(
                r#"{"type":"item.completed","item":{"id":"item_7","type":"todo_list","text":"update the plan"}}"#,
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
            emit(r#"{"type":"future","note":"first","note":"second"}"#);
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            completed();
        }
        "nested_duplicate_unknown_event_member" => {
            emit(r#"{"type":"future","items":[{"note":"first","note":"second"}]}"#);
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
            reasoning("reason-before-malformed", fixtures::PENDING_PROGRESS_TEXT);
            emit("{not-json");
        }
        "malformed_known_lifecycle" => {
            emit(r#"{"type":"item.started"}"#);
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
        "usage_partial_axes" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            emit(&format!(
                r#"{{"type":"turn.completed","usage":{{"cached_input_tokens":{},"output_tokens":{},"total_tokens":19}}}}"#,
                fixtures::CACHE_READ_INPUT_TOKENS,
                fixtures::OUTPUT_TOKENS
            ));
        }
        "usage_total_only" => {
            envelope(&format!(
                r#"{{"outcome":"completed","text":"{}","tool_calls":[]}}"#,
                fixtures::BUFFERED_ANSWER
            ));
            emit(r#"{"type":"turn.completed","usage":{"total_tokens":19}}"#);
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
            std::fs::write("fake-codex-selected-home", home.as_encoded_bytes())?;
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
                    r#"{"type":"item.updated","item":{"id":"busy-progress","type":"future_item"}}"#,
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

fn validate_argv() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().collect();
    std::fs::write("fake-codex-argv", arguments.join("\n"))?;
    let schema_index = arguments
        .iter()
        .position(|argument| argument == "--output-schema")
        .ok_or("missing --output-schema")?;
    let schema_path = arguments
        .get(schema_index + 1)
        .ok_or("missing output schema path")?;
    let output_index = arguments
        .iter()
        .position(|argument| argument == "--output-last-message")
        .ok_or("missing --output-last-message")?;
    let output_path = arguments
        .get(output_index + 1)
        .ok_or("missing output-last-message path")?;
    OUTPUT_LAST_MESSAGE_PATH
        .set(PathBuf::from(output_path))
        .map_err(|_| "output-last-message path was already set")?;
    let schema: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(schema_path)?)?;
    if schema["properties"]["outcome"].is_null() {
        return Err("unexpected output schema".into());
    }
    Ok(schema)
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
    retain_last_message(value);
    let escaped = json_escape(value);
    emit(&format!(
        r#"{{"type":"item.completed","item":{{"id":"{id}","type":"agent_message","text":"{escaped}"}}}}"#
    ));
}

fn retain_last_message(value: &str) {
    let Some(path) = OUTPUT_LAST_MESSAGE_PATH.get() else {
        eprintln!("output-last-message path was not validated");
        std::process::exit(2);
    };
    if let Err(error) = std::fs::write(path, value) {
        eprintln!("last message fixture could not be retained: {error}");
        std::process::exit(2);
    }
}

fn error_item(id: &str, message: &str) {
    emit(&format!(
        r#"{{"type":"item.completed","item":{{"id":"{id}","type":"error","message":"{}"}}}}"#,
        json_escape(message)
    ));
}

fn unsupported_item(id: &str, text: &str) {
    // An item type the adapter does not model; its `text` is dropped from the
    // output but must still seed the redaction lookbehind.
    emit(&format!(
        r#"{{"type":"item.completed","item":{{"id":"{id}","type":"diagnostic","text":"{}"}}}}"#,
        json_escape(text)
    ));
}

fn reasoning(id: &str, text: &str) {
    emit(&format!(
        r#"{{"type":"item.completed","item":{{"id":"{id}","type":"reasoning","text":"{}"}}}}"#,
        json_escape(text)
    ));
}

fn completed() {
    emit(&format!(
        r#"{{"type":"turn.completed","usage":{{"input_tokens":{},"cached_input_tokens":{},"cache_write_input_tokens":{},"output_tokens":{},"reasoning_output_tokens":3}}}}"#,
        fixtures::INPUT_TOKENS,
        fixtures::CACHE_READ_INPUT_TOKENS,
        fixtures::CACHE_CREATION_INPUT_TOKENS,
        fixtures::OUTPUT_TOKENS
    ));
}

fn completed_without_cache() {
    emit(&format!(
        r#"{{"type":"turn.completed","usage":{{"input_tokens":{},"output_tokens":{},"reasoning_output_tokens":3}}}}"#,
        fixtures::INPUT_TOKENS,
        fixtures::OUTPUT_TOKENS
    ));
}

fn deeply_nested_arguments() -> String {
    let mut value = "{}".to_string();
    for _ in 0..130 {
        value = format!(r#"{{"nested":{value}}}"#);
    }
    value
}

fn failed(message: &str) -> ! {
    emit(&format!(
        r#"{{"type":"turn.failed","error":{{"message":"{}"}}}}"#,
        json_escape(message)
    ));
    std::process::exit(1);
}

/// The pinned CLI's observed failure sequencing: a stream-level `error`
/// event first, then the `turn.failed` lifecycle echo, then a nonzero exit.
fn unrecoverable(message: &str) -> ! {
    emit_error(message);
    failed(message)
}

fn emit_error(message: &str) {
    emit(&format!(
        r#"{{"type":"error","message":"{}"}}"#,
        json_escape(message)
    ));
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn emit(line: &str) {
    println!("{line}");
}
