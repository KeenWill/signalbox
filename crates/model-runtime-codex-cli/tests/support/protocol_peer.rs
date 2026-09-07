//! In-memory peer for the one-thread, one-turn protocol conversation.

use serde_json::{Value, json};

pub(crate) fn response(request: &Value) -> Value {
    let result = match request["method"].as_str().expect("request method") {
        "initialize" => json!({"userAgent":"fake","codexHome":"/private/credential/home"}),
        "thread/start" => json!({"thread":{"id":"thread-fixture"}}),
        "turn/start" => json!({"turn":{"id":"turn-fixture","status":"inProgress","items":[]}}),
        method => panic!("unexpected client request: {method}"),
    };
    json!({"id":request["id"],"result":result})
}

pub(crate) fn notification(method: &str, mut params: Value) -> Value {
    params["threadId"] = json!("thread-fixture");
    params["turnId"] = json!("turn-fixture");
    json!({"method":method,"params":params})
}

pub(crate) fn turn(status: &str, info: Value) -> Value {
    notification(
        "turn/completed",
        json!({"turn":{
            "id":"turn-fixture","status":status,"items":[],
            "error":{"message":"provider diagnostic","codexErrorInfo":info}
        }}),
    )
}
