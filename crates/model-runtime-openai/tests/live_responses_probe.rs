//! Temporary measurements of Responses replay and inline compaction.
//! Runs only in the credentialed OpenAI smoke job. Logs structural evidence,
//! never credentials, response text, or encrypted content.

use std::time::Duration;

use serde_json::{Value, json};
use signalbox_model_runtime::{
    CredentialAccess, CredentialAccessError, CredentialAccessFailure, CredentialReference,
    CredentialValue,
};

// A configured model family on which the probe measures all six questions.
const MODEL: &str = "gpt-5.4";
// Bounds probe spend and each HTTP exchange, independently of answer quality.
const OUTPUT_CEILING: u32 = 1024;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(120);

struct EnvironmentCredential {
    variable: &'static str,
}

impl CredentialAccess for EnvironmentCredential {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        match std::env::var(self.variable) {
            Ok(value) if !value.is_empty() => Ok(CredentialValue::new(value.into_bytes())),
            _ => Err(CredentialAccessError::new(
                reference.clone(),
                CredentialAccessFailure::Unavailable,
            )),
        }
    }
}

struct ProbeResponse {
    status: u16,
    body: Value,
}

impl ProbeResponse {
    fn items(&self, kind: &str) -> Vec<Value> {
        self.body["output"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == kind)
            .cloned()
            .collect()
    }

    fn evidence(&self) -> Value {
        json!({
            "http_status": self.status,
            "response_status": self.body["status"],
            "model": self.body["model"],
            "error_code": self.body["error"]["code"],
            "error_type": self.body["error"]["type"],
            "error_param": self.body["error"]["param"],
            "usage": self.body["usage"],
            "reasoning_items": self.items("reasoning").len(),
            "compaction_items": self.items("compaction").len(),
        })
    }

    fn accepted(&self) -> bool {
        self.status == 200
            && matches!(
                self.body["status"].as_str(),
                Some("completed" | "incomplete")
            )
    }
}

fn request(input: Value, effort: &str) -> Value {
    json!({
        "model": MODEL,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "max_output_tokens": OUTPUT_CEILING,
        "reasoning": {"effort": effort},
        "input": input,
    })
}

async fn send(client: &reqwest::Client, key: &CredentialValue, body: Value) -> ProbeResponse {
    let key = String::from_utf8_lossy(key.expose_bytes());
    let response = client
        .post("https://api.openai.com/v1/responses")
        .bearer_auth(key.as_ref())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await;
    let Ok(response) = response else {
        return ProbeResponse {
            status: 0,
            body: json!({"error": "transport_failure"}),
        };
    };
    let status = response.status().as_u16();
    let body = match response.bytes().await {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({"error": "non_json_response"}))
        }
        Err(_) => json!({"error": "body_read_failure"}),
    };
    ProbeResponse { status, body }
}

fn report(letter: char, answer: &str, evidence: Value, keys: &[CredentialValue]) {
    let mut line = format!("PROBE {letter} {answer} {evidence}");
    for key in keys {
        let key = String::from_utf8_lossy(key.expose_bytes());
        line = line.replace(key.as_ref(), "[REDACTED]");
        // JSON encoding must not make a credential in an error message printable.
        let encoded = json!(key).to_string();
        line = line.replace(&encoded[1..encoded.len() - 1], "[REDACTED]");
    }
    println!("{line}");
}

#[tokio::test]
#[ignore = "spends one real OpenAI exchange; run only from the gated compatibility smoke"]
async fn the_responses_api_answers_replay_and_compaction_probes() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(EXCHANGE_TIMEOUT)
        .build()
        .expect("probe HTTP client builds");
    let mut keys = vec![
        EnvironmentCredential {
            variable: "OPENAI_API_KEY",
        }
        .resolve(&CredentialReference::new("openai-smoke"))
        .await
        .expect("the gated environment supplies OPENAI_API_KEY"),
    ];
    let mut other_keys = Vec::new();
    for (scope, variable) in [
        ("same_organization", "OPENAI_PROBE_SAME_ORG_API_KEY"),
        ("different_organization", "OPENAI_PROBE_OTHER_ORG_API_KEY"),
    ] {
        if let Ok(key) = (EnvironmentCredential { variable })
            .resolve(&CredentialReference::new(scope))
            .await
        {
            other_keys.push((scope, keys.len()));
            keys.push(key);
        }
    }

    // The UUID is caller-minted, deliberately lacking the provider's call prefix.
    let mut call_request = request(
        json!([
            {"role": "user", "content": "Use the tool result to reply with the single word ready."},
            {"type": "function_call", "call_id": "24a00000-0000-4000-8000-000000000001",
             "name": "probe_ready", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "24a00000-0000-4000-8000-000000000001",
             "output": "ready"}
        ]),
        "medium",
    );
    call_request["tools"] = json!([{"type": "function", "name": "probe_ready",
        "parameters": {"type": "object", "properties": {}}, "strict": false}]);
    let call = send(&client, &keys[0], call_request).await;
    report(
        'a',
        if call.accepted() {
            "accepted"
        } else {
            "not_accepted"
        },
        json!({"caller_minted_uuid": true, "tools_with_effort": "medium", "response": call.evidence()}),
        &keys,
    );

    let seed_input = json!([{"role": "user", "content": "Compute 37 times 43, check it, and reply with only the number."}]);
    let seed = send(&client, &keys[0], request(seed_input.clone(), "medium")).await;
    let reasoning = seed.items("reasoning");
    let mut portability = json!({"producer": seed.evidence(),
        "same_organization": "unmeasured_missing_credential",
        "different_organization": "unmeasured_missing_credential"});
    if reasoning.iter().any(|item| {
        item["encrypted_content"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    }) {
        let mut input = seed_input.as_array().expect("array fixture").clone();
        input.extend(
            seed.body["output"]
                .as_array()
                .expect("output items")
                .clone(),
        );
        input.push(json!({"role": "user", "content": "Reply with the same number."}));
        let replay = request(json!(input), "medium");
        let control = send(&client, &keys[0], replay.clone()).await;
        portability["same_credential_control"] = control.evidence();
        for (scope, index) in other_keys {
            let result = send(&client, &keys[index], replay.clone()).await;
            portability[scope] = result.evidence();
        }
    } else {
        portability["same_credential_control"] = json!("unmeasured_no_encrypted_reasoning");
    }
    report('b', "measured_scopes", portability, &keys);

    // More input than the documented minimum threshold; bounded omission probe.
    let long_input = json!([{"role": "user", "content": format!(
        "Retain this archive for later. {} Reply with the single word ready.",
        "The archive records an ordinary quiet day with clear skies. ".repeat(400))}]);
    let baseline = send(&client, &keys[0], request(long_input.clone(), "none")).await;
    let mut omitted_request = request(long_input.clone(), "none");
    omitted_request["context_management"] = json!([{"type": "compaction"}]);
    let omitted = send(&client, &keys[0], omitted_request).await;
    report(
        'c',
        if !omitted.items("compaction").is_empty() {
            "emitted"
        } else if omitted.accepted() {
            "not_emitted_at_tested_input"
        } else {
            "not_accepted"
        },
        json!({"baseline": baseline.evidence(), "without_threshold": omitted.evidence()}),
        &keys,
    );

    let mut explicit_request = request(long_input.clone(), "none");
    explicit_request["context_management"] =
        json!([{"type": "compaction", "compact_threshold": 1000}]);
    let compacted = send(&client, &keys[0], explicit_request).await;
    let mut d = json!({"producer": compacted.evidence()});
    let mut e = json!({"baseline": baseline.evidence(), "with_threshold": compacted.evidence()});
    if let Some(item) = compacted.items("compaction").first() {
        let mut with_creator = item.clone();
        let creator_was_returned = with_creator.get("created_by").is_some();
        if !creator_was_returned {
            with_creator["created_by"] = json!("probe");
        }
        let mut without_creator = with_creator.clone();
        without_creator
            .as_object_mut()
            .expect("compaction object")
            .remove("created_by");
        let next = json!({"role": "user", "content": "Reply with the single word ready."});
        let control = send(
            &client,
            &keys[0],
            request(json!([without_creator, next.clone()]), "none"),
        )
        .await;
        let result = send(
            &client,
            &keys[0],
            request(json!([with_creator, next]), "none"),
        )
        .await;
        d["created_by_returned"] = json!(creator_was_returned);
        d["without_created_by"] = control.evidence();
        d["with_created_by"] = result.evidence();
        e["compacted_item_only_replay"] = control.evidence();
        report(
            'd',
            if result.accepted() {
                "accepted"
            } else {
                "not_accepted"
            },
            d,
            &keys,
        );
    } else {
        report('d', "unmeasured_no_compaction_item", d, &keys);
    }
    report('e', "token_accounting_observations", e, &keys);

    let no_effort = send(&client, &keys[0], request(seed_input, "none")).await;
    let items = no_effort.items("reasoning");
    report(
        'f',
        if !no_effort.accepted() {
            "not_accepted"
        } else if items.is_empty() {
            "no_reasoning_item"
        } else {
            "reasoning_item_emitted"
        },
        json!({"response": no_effort.evidence(), "encrypted_items": items.iter()
            .filter(|item| item["encrypted_content"].as_str().is_some_and(|text| !text.is_empty())).count()}),
        &keys,
    );
}
