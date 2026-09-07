use serde_json::{Value, json};
use signalbox_model_runtime::{ProviderErrorKind, REDACTED, RedactingSink};

use super::classify::{FailureClass, classify, input_too_large};
use super::client::{Client, Event};
use super::decode::{fold_uninterpreted, parse};
use super::frame::{
    CodexErrorInfo, TextInput, TextInputKind, ThreadOptions, TurnInput, TurnStatus,
};

#[path = "../../tests/support/fake_codex_app_server.rs"]
mod peer;

fn new_client() -> Client {
    Client::new(
        ThreadOptions {
            model: "fixture-model".into(),
            cwd: "/workspace".into(),
            service_tier: None,
        },
        TurnInput {
            input: vec![TextInput {
                kind: TextInputKind::Text,
                text: "fixture prompt".into(),
            }],
            output_schema: json!({"type":"object"}),
            effort: None,
        },
    )
}

fn next(client: &mut Client) -> Value {
    parse(&client.take_stdin_frame().expect("client frame")).expect("valid client JSON")
}

fn running() -> Client {
    let mut client = new_client();
    let initialize = next(&mut client);
    client
        .receive(&peer::response(&initialize))
        .expect("initialize");
    next(&mut client);
    let thread = next(&mut client);
    client
        .receive(&peer::response(&thread))
        .expect("thread start");
    let turn = next(&mut client);
    client.receive(&peer::response(&turn)).expect("turn start");
    client
}

#[test]
fn handshake_creates_one_ephemeral_read_only_thread_and_one_turn() {
    let mut client = new_client();
    let initialize = next(&mut client);
    assert_eq!(initialize["method"], "initialize");
    assert!(initialize["params"].get("capabilities").is_none());
    client
        .receive(&peer::response(&initialize))
        .expect("initialize response");
    assert_eq!(next(&mut client), json!({"method":"initialized"}));
    let thread = next(&mut client);
    assert_eq!(thread["method"], "thread/start");
    assert_eq!(
        thread["params"],
        json!({"model":"fixture-model","cwd":"/workspace",
        "sandbox":"read-only","approvalPolicy":"never","ephemeral":true})
    );
    let Event::ThreadStarted(id) = client
        .receive(&peer::response(&thread))
        .expect("thread response")
    else {
        panic!("thread id must be published");
    };
    assert_eq!(id, "thread-fixture");
    let turn = next(&mut client);
    assert_eq!(turn["method"], "turn/start");
    assert_eq!(turn["params"]["threadId"], id);
    assert_eq!(turn["params"]["outputSchema"], json!({"type":"object"}));
    assert_eq!(turn["params"]["input"][0]["text"], "fixture prompt");
    client
        .receive(&peer::response(&turn))
        .expect("turn response");
    let Event::Terminal(turn) = client
        .receive(&peer::turn("completed", Value::Null))
        .expect("completion")
    else {
        panic!("completed turn closes conversation");
    };
    assert_eq!(turn.status, TurnStatus::Completed);
    assert!(client.is_terminal());
    assert!(
        client.take_stdin_frame().is_none(),
        "no second turn or resume"
    );
}

#[test]
fn failed_turns_classify_only_the_typed_error_variant() {
    use FailureClass::{PolicyRefusal, Provider};
    use ProviderErrorKind::*;
    let cases = [
        (
            json!("contextWindowExceeded"),
            Provider(RequestTooLarge),
            true,
        ),
        (
            json!("sessionBudgetExceeded"),
            Provider(Unrecognized),
            false,
        ),
        (json!("usageLimitExceeded"), Provider(QuotaExhausted), true),
        (json!("rateLimitExceeded"), Provider(RateLimited), true),
        (json!("serverOverloaded"), Provider(Overloaded), true),
        (
            json!("cyberPolicy"),
            PolicyRefusal(signalbox_model_runtime::RefusalReason::CyberPolicy),
            false,
        ),
        (
            json!("misalignmentPolicyViolation"),
            PolicyRefusal(signalbox_model_runtime::RefusalReason::Misalignment),
            false,
        ),
        (
            json!({"httpConnectionFailed":{"httpStatusCode":429}}),
            Provider(Unrecognized),
            true,
        ),
        (
            json!({"responseStreamConnectionFailed":{"httpStatusCode":503}}),
            Provider(Unrecognized),
            true,
        ),
        (
            json!("internalServerError"),
            Provider(ProviderInternal),
            true,
        ),
        (json!("unauthorized"), Provider(CredentialRejected), false),
        (json!("badRequest"), Provider(InvalidRequest), true),
        (json!("threadRollbackFailed"), Provider(Unrecognized), false),
        (json!("sandboxError"), Provider(Unrecognized), false),
        (
            json!({"responseStreamDisconnected":{"httpStatusCode":401}}),
            Provider(Unrecognized),
            false,
        ),
        (
            json!({"responseTooManyFailedAttempts":{"httpStatusCode":429}}),
            Provider(Unrecognized),
            false,
        ),
        (
            json!({"activeTurnNotSteerable":{"turnKind":"review"}}),
            Provider(Unrecognized),
            false,
        ),
        (json!("other"), Provider(Unrecognized), false),
        (Value::Null, Provider(Unrecognized), false),
        (
            json!({"futureError":{"diagnostic":"unauthorized quota exhausted"}}),
            Provider(Unrecognized),
            false,
        ),
    ];
    for (info, expected, proof) in cases {
        let mut client = running();
        let Event::Terminal(turn) = client
            .receive(&peer::turn("failed", info.clone()))
            .expect("typed failure")
        else {
            panic!("failure must close turn: {info}");
        };
        let error = turn.error.expect("failure detail");
        assert_eq!(error.message, "provider diagnostic");
        assert_eq!(
            classify(error.codex_error_info.as_ref()),
            expected,
            "{info}"
        );
        assert_eq!(
            client
                .activity
                .proves_non_acceptance(turn.status, error.codex_error_info.as_ref()),
            proof,
            "{info}"
        );
    }
}

#[test]
fn absent_error_info_does_not_infer_authentication_from_prose() {
    let mut client = running();
    let mut event = peer::turn("failed", Value::Null);
    event["params"]["turn"]["error"] = json!({"message":"invalid API key; rate limit exceeded"});
    let Event::Terminal(turn) = client.receive(&event).expect("failure without typed info") else {
        panic!("failed turn");
    };
    assert_eq!(
        classify(
            turn.error
                .as_ref()
                .and_then(|error| error.codex_error_info.as_ref())
        ),
        FailureClass::Provider(ProviderErrorKind::Unrecognized)
    );
}

#[test]
fn future_tags_and_http_statuses_are_retained_as_facts() {
    let unknown: CodexErrorInfo =
        serde_json::from_value(json!("futureError")).expect("additive error variant");
    assert_eq!(unknown.tag(), "futureError");
    assert_eq!(unknown.http_status(), None);
    let known: CodexErrorInfo =
        serde_json::from_value(json!({"httpConnectionFailed":{"httpStatusCode":429}}))
            .expect("status object");
    assert_eq!(known.tag(), "httpConnectionFailed");
    assert_eq!(known.http_status(), Some(429));
    assert_eq!(
        classify(Some(&known)),
        FailureClass::Provider(ProviderErrorKind::Unrecognized)
    );
}

#[test]
fn malformed_known_error_shapes_are_protocol_errors() {
    for info in [
        json!("httpConnectionFailed"),
        json!({"httpConnectionFailed":{"httpStatusCode":"429"}}),
        json!({"unauthorized":{},"other":{}}),
    ] {
        let mut client = running();
        assert!(
            client.receive(&peer::turn("failed", info.clone())).is_err(),
            "{info}"
        );
    }
}

#[test]
fn retry_telemetry_cannot_close_a_turn_or_prove_non_acceptance() {
    for (will_retry, proof) in [(true, false), (false, true)] {
        let mut client = running();
        let Event::Error(error) = client.receive(&peer::notification("error", json!({
            "willRetry":will_retry,"error":{"message":"retry diagnostic","codexErrorInfo":"usageLimitExceeded"}
        }))).expect("error notification") else { panic!("error telemetry"); };
        assert_eq!(error.error.message, "retry diagnostic");
        assert_eq!(error.will_retry, will_retry);
        assert!(!client.is_terminal(), "telemetry alone is never terminal");
        let Event::Terminal(turn) = client
            .receive(&peer::turn("failed", json!("usageLimitExceeded")))
            .expect("closure")
        else {
            panic!("closure");
        };
        assert_eq!(
            client.activity.proves_non_acceptance(
                turn.status,
                turn.error
                    .as_ref()
                    .and_then(|error| error.codex_error_info.as_ref())
            ),
            proof
        );
    }
}

#[test]
fn assistant_activity_independently_defeats_non_acceptance_proof() {
    let cases = [
        peer::notification(
            "item/started",
            json!({"item":{"type":"agentMessage","id":"message","text":""}}),
        ),
        peer::notification(
            "item/completed",
            json!({"item":{"type":"agentMessage","id":"message","text":"accepted"}}),
        ),
        peer::notification(
            "item/agentMessage/delta",
            json!({"itemId":"message","delta":"accepted"}),
        ),
        peer::notification(
            "thread/tokenUsage/updated",
            json!({"tokenUsage":{"total":{
                "inputTokens":1,"cachedInputTokens":0,"outputTokens":1,"reasoningOutputTokens":0,"totalTokens":2
            }}}),
        ),
        peer::notification(
            "thread/tokenUsage/updated",
            json!({"tokenUsage":{"total":{
                "inputTokens":1,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":1,"totalTokens":2
            }}}),
        ),
    ];
    for activity in cases {
        let mut client = running();
        client.receive(&activity).expect("assistant activity");
        let Event::Terminal(turn) = client
            .receive(&peer::turn("failed", json!("usageLimitExceeded")))
            .expect("failure")
        else {
            panic!("failed turn");
        };
        assert!(
            !client.activity.proves_non_acceptance(
                turn.status,
                turn.error
                    .as_ref()
                    .and_then(|error| error.codex_error_info.as_ref())
            ),
            "{activity}"
        );
    }
}

#[test]
fn every_positive_usage_axis_independently_defeats_non_acceptance_proof() {
    for axis in [
        "inputTokens",
        "cachedInputTokens",
        "cacheWriteInputTokens",
        "outputTokens",
        "reasoningOutputTokens",
        "totalTokens",
    ] {
        let mut client = running();
        let mut total = json!({"inputTokens":0,"cachedInputTokens":0,"cacheWriteInputTokens":0,
            "outputTokens":0,"reasoningOutputTokens":0,"totalTokens":0});
        total[axis] = json!(1);
        client
            .receive(&peer::notification(
                "thread/tokenUsage/updated",
                json!({
                    "tokenUsage":{"total":total}
                }),
            ))
            .expect("positive provider usage");
        let Event::Terminal(turn) = client
            .receive(&peer::turn("failed", json!("usageLimitExceeded")))
            .expect("failed turn")
        else {
            panic!("failure closes the turn");
        };
        assert!(
            !client.activity.proves_non_acceptance(
                turn.status,
                turn.error
                    .as_ref()
                    .and_then(|error| error.codex_error_info.as_ref())
            ),
            "positive {axis}"
        );
    }
}

#[test]
fn zero_usage_does_not_alone_defeat_non_acceptance_proof() {
    let mut client = running();
    client.receive(&peer::notification("thread/tokenUsage/updated", json!({
        "tokenUsage":{"total":{"inputTokens":0,"cachedInputTokens":0,"cacheWriteInputTokens":0,
            "outputTokens":0,"reasoningOutputTokens":0,"totalTokens":0}}
    }))).expect("zero usage");
    let Event::Terminal(turn) = client
        .receive(&peer::turn("failed", json!("usageLimitExceeded")))
        .expect("failed turn")
    else {
        panic!("failure closes the turn");
    };
    assert!(
        client.activity.proves_non_acceptance(
            turn.status,
            turn.error
                .as_ref()
                .and_then(|error| error.codex_error_info.as_ref())
        )
    );
}

#[test]
fn reasoning_activity_independently_defeats_non_acceptance_proof() {
    let cases = [
        peer::notification(
            "item/reasoning/summaryTextDelta",
            json!({"itemId":"reasoning-1","delta":"summary","summaryIndex":0}),
        ),
        peer::notification(
            "item/reasoning/textDelta",
            json!({"itemId":"reasoning-1","delta":"reasoning","contentIndex":0}),
        ),
        peer::notification(
            "item/started",
            json!({"item":{"type":"reasoning","id":"reasoning-1","summary":[],"content":[]}}),
        ),
        peer::notification(
            "item/completed",
            json!({"item":{"type":"reasoning","id":"reasoning-1","summary":["summary"],"content":["reasoning"]}}),
        ),
    ];
    for activity in cases {
        let mut client = running();
        assert!(matches!(
            client.receive(&activity).expect("reasoning notification"),
            Event::Ignored
        ));
        let Event::Terminal(turn) = client
            .receive(&peer::turn("failed", json!("usageLimitExceeded")))
            .expect("failed turn")
        else {
            panic!("failure closes the turn");
        };
        assert!(
            !client.activity.proves_non_acceptance(
                turn.status,
                turn.error
                    .as_ref()
                    .and_then(|error| error.codex_error_info.as_ref())
            ),
            "{activity}"
        );
    }
}

#[test]
fn reasoning_in_a_failed_turn_summary_defeats_non_acceptance_proof() {
    let mut client = running();
    let mut terminal = peer::turn("failed", json!("usageLimitExceeded"));
    terminal["params"]["turn"]["items"] =
        json!([{"type":"reasoning","id":"reasoning-1","summary":["summary"],"content":[]}]);
    let Event::Terminal(turn) = client.receive(&terminal).expect("failed turn") else {
        panic!("failure closes the turn");
    };
    assert!(
        !client.activity.proves_non_acceptance(
            turn.status,
            turn.error
                .as_ref()
                .and_then(|error| error.codex_error_info.as_ref())
        )
    );
}

#[test]
fn reasoning_in_a_nonterminal_turn_summary_defeats_later_non_acceptance_proof() {
    let mut client = running();
    let mut progress = peer::turn("inProgress", Value::Null);
    progress["params"]["turn"]["items"] =
        json!([{"type":"reasoning","id":"reasoning-1","summary":["summary"],"content":[]}]);
    assert!(matches!(
        client.receive(&progress).expect("in-progress summary"),
        Event::Ignored
    ));
    assert!(!client.is_terminal());
    let Event::Terminal(turn) = client
        .receive(&peer::turn("failed", json!("usageLimitExceeded")))
        .expect("failed turn")
    else {
        panic!("failure closes the turn");
    };
    assert!(
        !client.activity.proves_non_acceptance(
            turn.status,
            turn.error
                .as_ref()
                .and_then(|error| error.codex_error_info.as_ref())
        )
    );
}

#[test]
fn model_authored_items_defeat_proof_after_a_nonretrying_failure_notification() {
    for kind in [
        "plan",
        "commandExecution",
        "mcpToolCall",
        "fileChange",
        "dynamicToolCall",
        "collabAgentToolCall",
        "subAgentActivity",
        "webSearch",
        "imageView",
        "sleep",
        "imageGeneration",
        "enteredReviewMode",
        "exitedReviewMode",
        "contextCompaction",
        "functionCallOutput",
        "futureModelItem",
    ] {
        for method in ["item/started", "item/completed"] {
            let mut client = running();
            client
                .receive(&peer::notification(
                    method,
                    json!({"item":{"id":"model-item","type":kind}}),
                ))
                .expect("item notification");
            client
                .receive(&peer::notification(
                    "error",
                    json!({"willRetry":false,
                        "error":{"message":"quota failure","codexErrorInfo":"usageLimitExceeded"}
                    }),
                ))
                .expect("nonretrying failure telemetry");
            let Event::Terminal(turn) = client
                .receive(&peer::turn("failed", json!("usageLimitExceeded")))
                .expect("failed turn")
            else {
                panic!("failure closes the turn");
            };
            assert!(
                !client.activity.proves_non_acceptance(
                    turn.status,
                    turn.error
                        .as_ref()
                        .and_then(|error| error.codex_error_info.as_ref())
                ),
                "{method}: {kind}"
            );
        }
    }
}

#[test]
fn model_authored_turn_summary_items_defeat_non_acceptance_proof() {
    for kind in [
        "plan",
        "commandExecution",
        "mcpToolCall",
        "fileChange",
        "dynamicToolCall",
        "collabAgentToolCall",
        "subAgentActivity",
        "webSearch",
        "imageView",
        "sleep",
        "imageGeneration",
        "enteredReviewMode",
        "exitedReviewMode",
        "contextCompaction",
        "functionCallOutput",
        "futureModelItem",
    ] {
        let mut client = running();
        let mut terminal = peer::turn("failed", json!("usageLimitExceeded"));
        terminal["params"]["turn"]["items"] = json!([{"id":"model-item","type":kind}]);
        let Event::Terminal(turn) = client.receive(&terminal).expect("failed turn") else {
            panic!("failure closes the turn");
        };
        assert!(
            !client.activity.proves_non_acceptance(
                turn.status,
                turn.error
                    .as_ref()
                    .and_then(|error| error.codex_error_info.as_ref())
            ),
            "{kind}"
        );
    }
}

#[test]
fn input_only_items_do_not_alone_defeat_non_acceptance_proof() {
    for kind in ["userMessage", "hookPrompt"] {
        let mut client = running();
        let item = json!({"id":"input-item","type":kind});
        client
            .receive(&peer::notification("item/started", json!({"item":item})))
            .expect("input item starts");
        client
            .receive(&peer::notification("item/completed", json!({"item":item})))
            .expect("input item completes");
        let mut terminal = peer::turn("failed", json!("usageLimitExceeded"));
        terminal["params"]["turn"]["items"] = json!([item]);
        let Event::Terminal(turn) = client.receive(&terminal).expect("failed turn") else {
            panic!("failure closes the turn");
        };
        assert!(
            client.activity.proves_non_acceptance(
                turn.status,
                turn.error
                    .as_ref()
                    .and_then(|error| error.codex_error_info.as_ref())
            ),
            "{kind}"
        );
    }
}

#[test]
fn completed_or_interrupted_status_never_proves_non_acceptance() {
    for status in ["completed", "interrupted"] {
        let mut client = running();
        let Event::Terminal(turn) = client
            .receive(&peer::turn(status, json!("usageLimitExceeded")))
            .expect("terminal")
        else {
            panic!("terminal");
        };
        assert!(client.is_terminal());
        assert!(
            !client.activity.proves_non_acceptance(
                turn.status,
                turn.error
                    .as_ref()
                    .and_then(|error| error.codex_error_info.as_ref())
            )
        );
    }
}

#[test]
fn in_progress_completion_notification_is_not_terminal() {
    let mut client = running();
    assert!(matches!(
        client
            .receive(&peer::turn("inProgress", Value::Null))
            .expect("in-progress notification"),
        Event::Ignored
    ));
    assert!(!client.is_terminal());
    assert!(matches!(
        client
            .receive(&peer::turn("completed", Value::Null))
            .expect("completed notification"),
        Event::Terminal(_)
    ));
}

#[test]
fn unknown_server_requests_are_declined_without_stalling_the_turn() {
    let mut client = running();
    client.receive(&json!({"id":"refresh-id","method":"account/chatgptAuthTokens/refresh","params":{"reason":"expired"}})).expect("decline request");
    assert_eq!(
        next(&mut client),
        json!({"id":"refresh-id","error":{"code":-32601,"message":"Method not supported"}})
    );
    assert!(matches!(
        client
            .receive(&peer::turn("completed", Value::Null))
            .expect("turn still completes"),
        Event::Terminal(_)
    ));
}

fn pending_request(method: &str) -> (Client, Value) {
    let mut client = new_client();
    let mut request = next(&mut client);
    while request["method"] != method {
        client
            .receive(&peer::response(&request))
            .expect("preceding method succeeds");
        request = next(&mut client);
        if request["method"] == "initialized" {
            request = next(&mut client);
        }
    }
    (client, request)
}

#[test]
fn rpc_rejections_remain_local_except_typed_turn_input_too_large() {
    let cases = [
        ("initialize", -32001, Value::Null, false),
        (
            "thread/start",
            -32602,
            json!({"input_error_code":"input_too_large"}),
            false,
        ),
        ("turn/start", -32001, Value::Null, false),
        (
            "turn/start",
            -32602,
            json!({"input_error_code":"input_too_large"}),
            true,
        ),
        (
            "turn/start",
            -32602,
            json!({"input_error_code":"future_error"}),
            false,
        ),
    ];
    for (method, code, data, too_large) in cases {
        let (mut client, request) = pending_request(method);
        let Event::Rejected { method: rejected_method, error } = client.receive(&json!({
            "id":request["id"],"error":{"code":code,"message":"quota exceeded unauthorized", "data":data}
        })).expect("RPC rejection") else { panic!("local rejection"); };
        assert_eq!(rejected_method, method);
        assert_eq!(error.code, code);
        assert_eq!(input_too_large(rejected_method, &error), too_large);
        assert!(client.is_terminal());
    }
}

#[test]
fn agent_items_deltas_and_total_usage_are_decoded_without_projection_loss() {
    let mut client = running();
    let Event::AgentMessage { message, completed } = client
        .receive(&peer::notification(
            "item/completed",
            json!({
                "item":{"type":"agentMessage","id":"message-1","text":"final envelope"}
            }),
        ))
        .expect("agent message")
    else {
        panic!("agent message");
    };
    assert!(completed);
    assert_eq!(message.id, "message-1");
    assert_eq!(message.text, "final envelope");
    let Event::Delta(delta) = client
        .receive(&peer::notification(
            "item/agentMessage/delta",
            json!({"itemId":"message-1","delta":"delta text"}),
        ))
        .expect("delta")
    else {
        panic!("delta");
    };
    assert_eq!(delta.item_id, "message-1");
    assert_eq!(delta.delta, "delta text");
    let Event::Usage(usage) = client.receive(&peer::notification("thread/tokenUsage/updated", json!({"tokenUsage":{
        "total":{"inputTokens":100,"cachedInputTokens":20,"cacheWriteInputTokens":10,"outputTokens":5,"reasoningOutputTokens":3,"totalTokens":105},
        "last":{"inputTokens":1,"cachedInputTokens":0,"outputTokens":1,"reasoningOutputTokens":0,"totalTokens":2}
    }}))).expect("usage") else { panic!("total usage"); };
    let total = usage.token_usage.total;
    assert_eq!(
        (
            total.input_tokens,
            total.cached_input_tokens,
            total.cache_write_input_tokens,
            total.output_tokens,
            total.reasoning_output_tokens,
            total.total_tokens
        ),
        (100, 20, Some(10), 5, 3, 105)
    );
}

#[test]
fn correlation_rejects_cross_thread_or_cross_turn_notifications() {
    for (field, wrong_id) in [("threadId", "other-thread"), ("turnId", "other-turn")] {
        let mut client = running();
        let mut frame = peer::notification(
            "item/agentMessage/delta",
            json!({"itemId":"message","delta":"wrong conversation"}),
        );
        frame["params"][field] = json!(wrong_id);
        assert!(client.receive(&frame).is_err(), "{field}");
    }
}

#[test]
fn response_correlation_and_duplicate_members_fail_closed() {
    let mut client = new_client();
    assert!(client.receive(&json!({"id":2,"result":{}})).is_err());
    assert!(parse(br#"{"id":1,"result":{},"result":{"codexHome":"hidden"}}"#).is_err());
    assert!(parse(b"not JSON").is_err());
    assert!(parse(&[0xff]).is_err());
}

#[test]
fn unknown_notifications_skip_typed_parsing() {
    let mut client = running();
    assert!(matches!(
        client
            .receive(&json!({"method":"future/notification","params":[1,"arbitrary"]}))
            .expect("unknown payload is bounded JSON only"),
        Event::Ignored
    ));
    assert!(!client.is_terminal());
}

fn folded(value: Value, paths: &[&[&str]], following: &str) -> String {
    let mut observed = Vec::new();
    let mut sink: RedactingSink<'_, ()> = RedactingSink::new(&mut observed);
    fold_uninterpreted(&mut sink, &value, paths);
    sink.redact_terminal_failure_text(following)
}

#[test]
fn dropped_nested_metadata_governs_the_following_emitted_text() {
    let frame = json!({"method":"turn/completed","params":{"turn":{"error":{
        "message":"api_", "misalignment":{"steer":{"message":"key=fixture-secret"}}
    }}}});
    assert_eq!(
        folded(
            frame,
            &[
                &["method"],
                &[
                    "params",
                    "turn",
                    "error",
                    "misalignment",
                    "steer",
                    "message"
                ]
            ],
            "key=fixture-secret"
        ),
        REDACTED
    );
}

#[test]
fn dropped_units_preserve_array_adjacency_and_independent_object_markers() {
    for dropped in [
        json!(["api", ["_key="]]),
        json!({"marker":"api_key=","benign":"ordinary"}),
        json!({"nested":[{"marker":"api_key=","benign":"ordinary"}]}),
    ] {
        assert_eq!(
            folded(dropped.clone(), &[], "fixture-secret"),
            REDACTED,
            "{dropped}"
        );
    }
    assert_eq!(
        folded(json!({"a":"api_key=","b":"token="}), &[], "ordinary text"),
        REDACTED
    );
    assert_eq!(
        folded(json!({"keepalive":"ordinary"}), &[], "ordinary text"),
        "ordinary text"
    );
}

#[test]
fn member_paths_do_not_confuse_literal_slashes_with_nested_fields() {
    assert_eq!(
        folded(
            json!({"params/turn/error/message":"api_key=","params":{"turn":{"error":{"message":"ordinary"}}}}),
            &[&["params", "turn", "error", "message"]],
            "fixture-secret"
        ),
        REDACTED
    );
}
