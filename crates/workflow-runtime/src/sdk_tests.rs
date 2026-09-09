use super::*;
use deno_core::serde_json;
use deno_core::v8;

#[tokio::test(flavor = "current_thread")]
async fn emitted_typescript_entry_returns_checked_session_result() -> Result<(), Box<dyn Error>> {
    const COMMAND: &str = "12345678-1234-1234-1234-123456789abc";
    const MODEL: &str = "22345678-1234-1234-1234-123456789abc";
    const SESSION: &str = "32345678-1234-1234-1234-123456789abc";
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let (mut runtime, _) = isolate(sender)?;
    let module = runtime
        .load_main_es_module_from_code(
            &ModuleSpecifier::parse(PROGRAM_MAIN_SPECIFIER)?,
            include_str!("../tests/fixtures/session.js"),
        )
        .await?;
    let evaluation = runtime.mod_evaluate(module);
    runtime
        .run_event_loop(PollEventLoopOptions::default())
        .await?;
    evaluation.await?;
    let namespace = runtime.get_module_namespace(module)?;
    let entry = {
        deno_core::scope!(scope, runtime);
        let namespace = v8::Local::new(scope, namespace);
        let name = v8::String::new(scope, "default").expect("entrypoint name");
        let value = namespace.get(scope, name.into()).expect("default export");
        let function = v8::Local::<v8::Function>::try_from(value)?;
        v8::Global::new(scope, function)
    };
    let input = serde_json::to_vec(&serde_json::json!({ "command": COMMAND, "model": MODEL }))?;
    let input = runtime.execute_script(
        "fixture-input",
        format!("new Uint8Array({})", serde_json::to_string(&input)?),
    )?;
    let completion = runtime.call_with_args(&entry, &[input]);
    let answer = serde_json::to_vec(&serde_json::json!({ "session": SESSION }))?;
    let mut effect_requests = Vec::new();
    loop {
        let status = poll_runtime_once(&mut runtime).await;
        while let Ok(request) = receiver.try_recv() {
            effect_requests.push(request.kind.into_domain());
            request
                .reply
                .send(IsolateDelivery::Answer {
                    payload: answer.clone(),
                })
                .unwrap_or_else(|_| panic!("entrypoint must await its effect"));
        }
        if let Poll::Ready(result) = status {
            result?;
            break;
        }
        tokio::task::yield_now().await;
    }
    let result = completion.await?;
    let bytes = {
        deno_core::scope!(scope, runtime);
        let value = v8::Local::new(scope, result);
        let value = v8::Local::<v8::Uint8Array>::try_from(value)?;
        let mut bytes = vec![0; value.byte_length()];
        value.copy_contents(&mut bytes);
        bytes
    };
    assert_eq!(
        bytes, answer,
        "the emitted entrypoint returns its encoded typed result"
    );
    assert_eq!(effect_requests.len(), 1);
    let RequestKind::Effect(request) = &effect_requests[0] else {
        panic!("expected a session effect")
    };
    assert_eq!(request.method(), "create");
    let wire: serde_json::Value = serde_json::from_slice(request.payload().as_bytes())?;
    assert_eq!(
        wire,
        serde_json::json!({ "command": COMMAND, "model": MODEL })
    );
    Ok(())
}

/// Executes SDK calls inside the closed isolate and supplies exact scripted answers.
async fn sdk_script(
    source: &str,
    answers: impl IntoIterator<Item = IsolateDelivery>,
) -> (Result<(), Box<dyn Error>>, Vec<RequestKind>) {
    let mut observed = Vec::new();
    let result = async {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let (mut runtime, _) = isolate(sender)?;
        let specifier = ModuleSpecifier::parse(PROGRAM_MAIN_SPECIFIER)?;
        let module = runtime
            .load_main_es_module_from_code(
                &specifier,
                format!("import * as sdk from {PROGRAM_SDK_V1_SPECIFIER:?};\n{source}"),
            )
            .await?;
        let evaluation = runtime.mod_evaluate(module);
        let mut answers = answers.into_iter();
        loop {
            let status = poll_runtime_once(&mut runtime).await;
            while let Ok(request) = receiver.try_recv() {
                observed.push(request.kind.into_domain());
                request
                    .reply
                    .send(answers.next().expect("scripted answer for every request"))
                    .unwrap_or_else(|_| panic!("isolate must retain the request receiver"));
            }
            if let Poll::Ready(result) = status {
                result?;
                break;
            }
            tokio::task::yield_now().await;
        }
        evaluation.await?;
        assert!(
            answers.next().is_none(),
            "all scripted answers were consumed"
        );
        Ok(())
    }
    .await;
    (result, observed)
}

#[tokio::test(flavor = "current_thread")]
async fn input_codec_refuses_invalid_data_before_program_body() {
    let (result, requests) = sdk_script(
        r#"
const input = sdk.jsonCodec(value => {
  if (typeof value !== "string") throw new TypeError("expected string input");
  return value;
});
const program = sdk.defineProgram({ input, output: input,
  run: () => sdk.now(new Uint8Array()) });
await program(new Uint8Array([49]));
"#,
        [],
    )
    .await;
    assert!(
        result
            .expect_err("wrong input must fail admission")
            .to_string()
            .contains("expected string input")
    );
    assert!(
        requests.is_empty(),
        "invalid input must not execute program effects"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn output_codec_refuses_invalid_program_result() {
    let (result, requests) = sdk_script(
        r#"
const codec = sdk.jsonCodec(value => {
  if (typeof value !== "string") throw new TypeError("expected string result");
  return value;
});
await sdk.defineProgram({ input: codec, output: codec, run: () => 1 })(codec.encode("input"));
"#,
        [],
    )
    .await;
    assert!(
        result
            .expect_err("wrong output must fail encoding")
            .to_string()
            .contains("expected string result")
    );
    assert!(requests.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn json_codec_serializes_checked_proxy_descriptors() {
    let (result, requests) = sdk_script(
        r#"
const object = new Proxy({ correct: true }, {
  get(target, key) { return key === "correct" ? false : Reflect.get(target, key); }
});
const array = new Proxy([true], {
  get(target, key) { return key === "0" ? false : Reflect.get(target, key); }
});
const child = { correct: true };
const moving = new Proxy({ child, later: true }, {
  getOwnPropertyDescriptor(target, key) {
    if (key === "later") child.correct = false;
    return Object.getOwnPropertyDescriptor(target, key);
  }
});
const encoded = sdk.jsonCodec(value => value).encode({ object, array, moving });
if (String.fromCharCode(...encoded) !== '{"object":{"correct":true},"array":[true],"moving":{"child":{"correct":true},"later":true}}') {
  throw new Error("serialization must use checked descriptors without rereading proxy properties");
}
"#,
        [],
    )
    .await;
    result.expect("proxy property reads must not replace the checked data");
    assert!(requests.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn json_codec_preserves_its_contract_when_all_codec_globals_are_replaced() {
    let (result, requests) = sdk_script(
        r#"
const descriptor = Object.getOwnPropertyDescriptor;
const define = Object.defineProperty;
const remove = Reflect.deleteProperty;
const fromCode = String.fromCharCode;
const TestError = Error;
const typedArrayPrototype = Object.getPrototypeOf(Uint8Array.prototype);
const byteLength = Function.prototype.call.bind(descriptor(typedArrayPrototype, "length").get);
const input = new Uint8Array([34, 233, 155, 170, 240, 159, 152, 128, 34]);
const invalidInputs = [new Uint8Array([255]), new Uint8Array([123]), new Uint16Array([49]), { 0: 49, length: 1 }];
const cyclic = {}; cyclic.self = cyclic;
const invalid = [NaN, -0, { missing: undefined }, [undefined], new Array(1), cyclic];
const targets = [
  [JSON, "parse"], [JSON, "stringify"],
  [globalThis, "TextEncoder"], [globalThis, "TextDecoder"],
  [globalThis, "decodeURIComponent"], [Uint8Array, "from"],
  [Uint8Array, Symbol.hasInstance],
  [typedArrayPrototype, "length"], [typedArrayPrototype, Symbol.toStringTag],
  [typedArrayPrototype, Symbol.iterator],
  [String.prototype, "replace"], [String.prototype, "charCodeAt"],
  [String.prototype, "padStart"], [String.prototype, Symbol.iterator],
  [Number.prototype, "toString"], [Number, "isFinite"], [Number, "isInteger"],
  [Array, "from"], [Array, "isArray"], [Array.prototype, "join"],
  [Array.prototype, "every"], [Array.prototype, Symbol.iterator],
  [Object.prototype, "toJSON"], [Array.prototype, "toJSON"],
  [Object, "freeze"], [Object, "create"], [Object, "defineProperty"],
  [Object, "getPrototypeOf"], [Object, "setPrototypeOf"],
  [Object, "getOwnPropertyDescriptor"], [Object, "hasOwn"], [Object, "is"],
  [Reflect, "ownKeys"], [Set.prototype, "has"], [Set.prototype, "add"],
  [Set.prototype, "delete"], [Function.prototype, "call"], [Function.prototype, "bind"],
  [globalThis, "JSON"], [globalThis, "Uint8Array"], [globalThis, "Object"],
  [globalThis, "Array"], [globalThis, "String"], [globalThis, "Number"],
  [globalThis, "Set"], [globalThis, "TypeError"],
];
const saved = [];
for (let index = 0; index < targets.length; index++) {
  saved[index] = descriptor(targets[index][0], targets[index][1]);
}
try {
  for (let index = 0; index < targets.length; index++) {
    define(targets[index][0], targets[index][1], {
      value: () => "null", writable: true, configurable: true
    });
  }
  const codec = sdk.jsonCodec(value => value);
  const encoded = codec.encode({ correct: "雪😀", values: [true, 1.5, null] });
  let text = "";
  for (let index = 0; index < byteLength(encoded); index++) text += fromCode(encoded[index]);
  if (text !== '{"correct":"\\u96ea\\ud83d\\ude00","values":[true,1.5,null]}') {
    throw new TestError("replaced globals must not change checked JSON bytes");
  }
  if (codec.decode(input) !== "雪😀") {
    throw new TestError("replaced globals must not change UTF-8 input decoding");
  }
  for (let index = 0; index < invalid.length; index++) {
    let rejected = false;
    try { codec.encode(invalid[index]); } catch { rejected = true; }
    if (!rejected) throw new TestError("replaced globals bypassed JSON validation for case " + index);
  }
  for (let index = 0; index < invalidInputs.length; index++) {
    let rejected = false;
    try { codec.decode(invalidInputs[index]); } catch { rejected = true; }
    if (!rejected) throw new TestError("replaced globals bypassed input validation for case " + index);
  }
} finally {
  for (let index = 0; index < targets.length; index++) {
    if (saved[index] === undefined) remove(targets[index][0], targets[index][1]);
    else define(targets[index][0], targets[index][1], saved[index]);
  }
}
"#,
        [],
    )
    .await;
    result.expect(
        "simultaneous intrinsic replacement must preserve codec encoding, decoding and rejection",
    );
    assert!(requests.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn json_codec_encoding_uses_the_preloaded_stringifier() {
    let (result, requests) = sdk_script(
        r#"
const codec = sdk.jsonCodec(value => {
  JSON.stringify = () => '{"corrupted":true}';
  return value;
});
const encoded = codec.encode({ correct: true });
if (String.fromCharCode(...encoded) !== '{"correct":true}') {
  throw new Error("a codec callback must not replace the SDK JSON stringifier");
}
"#,
        [],
    )
    .await;
    result.expect("encoding must preserve the validated value after JSON.stringify is reassigned");
    assert!(requests.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn json_codec_decoding_uses_the_preloaded_parser() {
    let (result, requests) = sdk_script(
        r#"
JSON.parse = () => ({ corrupted: true });
const codec = sdk.jsonCodec(value => value);
const decoded = codec.decode(Uint8Array.from('{"correct":true}', c => c.charCodeAt(0)));
if (decoded.correct !== true || Object.hasOwn(decoded, "corrupted")) {
  throw new Error("program code must not replace the SDK JSON parser");
}
"#,
        [],
    )
    .await;
    result.expect("decoding must read the input bytes after JSON.parse is reassigned");
    assert!(requests.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn json_codec_ignores_inherited_serialization_hooks() {
    for (setup, value, expected) in [
        (
            "Object.prototype.toJSON = () => null;",
            "({ correct: true })",
            r#"{"correct":true}"#,
        ),
        ("Object.prototype.toJSON = () => null;", "[true]", "[true]"),
        (
            "Array.prototype.toJSON = () => null;",
            "({ nested: [true] })",
            r#"{"nested":[true]}"#,
        ),
        (
            "Object.defineProperty(Object.prototype, 'toJSON', { get() { throw new Error('hook was invoked'); } });",
            "({ correct: true })",
            r#"{"correct":true}"#,
        ),
    ] {
        let (result, requests) = sdk_script(
            &format!(
                "{setup}\nconst encoded = sdk.jsonCodec(value => value).encode({value});\n\
                 if (String.fromCharCode(...encoded) !== {expected:?}) throw new Error('inherited hook changed encoded data');"
            ),
            [],
        )
        .await;
        result.unwrap_or_else(|error| {
            panic!("{setup} must not change {value} or invoke the hook: {error}")
        });
        assert!(requests.is_empty());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn json_codec_decoding_uses_the_preloaded_uri_decoder() {
    let (result, requests) = sdk_script(
        r#"
globalThis.decodeURIComponent = () => '{"corrupted":true}';
const codec = sdk.jsonCodec(value => value);
const decoded = codec.decode(Uint8Array.from('{"correct":true}', c => c.charCodeAt(0)));
if (decoded.correct !== true || Object.hasOwn(decoded, "corrupted")) {
  throw new Error("program code must not replace the SDK URI decoder");
}
"#,
        [],
    )
    .await;
    result.expect("decoding must read the input bytes after decodeURIComponent is reassigned");
    assert!(requests.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn json_codec_refuses_values_that_json_cannot_preserve() {
    for value in [
        "NaN",
        "Infinity",
        "-Infinity",
        "-0",
        "({ nested: { missing: undefined } })",
        "[1, [NaN]]",
        "[undefined]",
        "new Array(1)",
        "({ callback() {} })",
        "({ value: Symbol('value') })",
        "({ value: 1n })",
        "({ toJSON() { return null; } })",
        "new Number(1)",
        "Object.assign([1], { extra: 2 })",
        "({ [Symbol('key')]: 1 })",
        "Object.defineProperty({}, 'hidden', { value: 1 })",
        "({ get changing() { return 1; } })",
        "(() => { const value = {}; value.self = value; return value; })()",
    ] {
        let (result, requests) = sdk_script(
            &format!("sdk.jsonCodec(value => value).encode({value});"),
            [],
        )
        .await;
        assert!(
            result.is_err(),
            "encoding {value} must refuse a value JSON would change or omit"
        );
        assert!(requests.is_empty());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn json_codec_preserves_nested_json_values() {
    let (result, requests) = sdk_script(
        r#"
const codec = sdk.jsonCodec(value => value);
const shared = { text: "雪😀" };
const value = { values: [null, true, false, 0, 1.5, "", shared], repeated: shared };
const decoded = codec.decode(codec.encode(value));
if (JSON.stringify(decoded) !== '{"values":[null,true,false,0,1.5,"",{"text":"雪😀"}],"repeated":{"text":"雪😀"}}') {
  throw new Error("nested JSON values must round trip without loss");
}
"#,
        [],
    )
    .await;
    result.expect("JSON data including shared subobjects must remain encodable");
    assert!(requests.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn session_wrapper_refuses_invalid_input_before_requesting_effect() {
    let (result, requests) = sdk_script(
        r#"await sdk.session.create({ command: "not a UUID", model: "not a UUID" });"#,
        [],
    )
    .await;
    assert!(
        result
            .expect_err("invalid effect input must fail")
            .to_string()
            .contains("expected a UUID")
    );
    assert!(
        requests.is_empty(),
        "invalid method data must not reach the host"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_wrapper_refuses_malformed_host_answer() {
    let (result, requests) = sdk_script(
        r#"
const identity = "12345678-1234-1234-1234-123456789abc";
await sdk.session.create({ command: identity, model: identity });
"#,
        [IsolateDelivery::Answer {
            payload: br#"{"session":17}"#.to_vec(),
        }],
    )
    .await;
    assert!(
        result
            .expect_err("invalid effect answer must fail decoding")
            .to_string()
            .contains("expected a Unicode string")
    );
    assert_eq!(requests.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn session_turn_preserves_full_width_version_and_unicode() {
    let (result, requests) = sdk_script(
        r#"
const identity = "12345678-1234-1234-1234-123456789abc";
const answer = await sdk.session.turn({ command: identity, session: identity,
  text: "雪😀", defaults_version: "18446744073709551615" });
if (answer.kind !== "answer" || answer.value.outcome !== "refused") throw new Error("expected refusal");
"#,
        [IsolateDelivery::Answer { payload: br#"{"outcome":"refused"}"#.to_vec() }],
    )
    .await;
    result.expect("the maximum u64 must encode exactly");
    let RequestKind::Effect(request) = &requests[0] else {
        panic!("expected a session effect")
    };
    let wire: serde_json::Value =
        serde_json::from_slice(request.payload().as_bytes()).expect("valid JSON");
    assert_eq!(wire["defaults_version"].as_u64(), Some(u64::MAX));
    assert_eq!(wire["text"], "雪😀");
    assert_eq!(request.method(), "turn");
}

#[tokio::test(flavor = "current_thread")]
async fn registration_wrapper_preserves_grants_and_artifact_bytes() {
    let (result, requests) = sdk_script(
        r#"
const identity = "12345678-1234-1234-1234-123456789abc";
const result = await sdk.register({ id: identity, name: "example", revision: "revision",
  source: [0, 255], artifact: "export {}; // 雪", grants: ["session"] });
if (result.kind !== "reject" || result.reason !== "capability_denied") throw new Error("expected grant refusal");
"#,
        [IsolateDelivery::Reject { reason: IsolateRejectReason::CapabilityDenied }],
    )
    .await;
    result.expect("grant refusals remain typed deliveries");
    let RequestKind::Effect(request) = &requests[0] else {
        panic!("expected a registration effect")
    };
    let wire: serde_json::Value =
        serde_json::from_slice(request.payload().as_bytes()).expect("valid JSON");
    assert_eq!(wire["source"], serde_json::json!([0, 255]));
    assert_eq!(wire["artifact"], "export {}; // 雪");
    assert_eq!(wire["grants"], serde_json::json!(["session"]));
    assert_eq!(request.method(), "register");
}
