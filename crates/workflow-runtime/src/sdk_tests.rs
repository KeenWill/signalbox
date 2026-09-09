use super::*;
use deno_core::serde_json;

// The isolate bridge strips delivery ordinals; these fixtures need any positive ordinal.
const SCRIPTED_REQUEST: RequestOrdinal = RequestOrdinal::try_from_u64(1).expect("positive ordinal");

/// Executes SDK calls inside the closed isolate and supplies exact scripted answers.
async fn sdk_script(
    source: &str,
    answers: impl IntoIterator<Item = DeliveryKind>,
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
                observed.push(request.kind);
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
async fn input_codec_refuses_inherited_required_fields() {
    let (result, requests) = sdk_script(
        r#"
const identity = "12345678-1234-1234-1234-123456789abc";
Object.prototype.command = identity;
const input = sdk.jsonCodec(value => {
  if (typeof value !== "object" || value === null || !("command" in value)
    || !("model" in value) || typeof value.command !== "string" || typeof value.model !== "string") {
    throw new TypeError("expected command and model input fields");
  }
  return { command: value.command, model: value.model };
});
const bytes = value => Uint8Array.from(JSON.stringify(value), c => c.charCodeAt(0));
const valid = input.decode(bytes({ command: identity, model: identity }));
if (valid.command !== identity || valid.model !== identity) throw new Error("own input fields must decode");
const program = sdk.defineProgram({ input, output: input,
  run: () => { throw new Error("inherited input reached the program body"); } });
await program(bytes({ model: identity }));
"#,
        [],
    )
    .await;
    assert!(
        result
            .expect_err("a required input field must occur in the durable bytes")
            .to_string()
            .contains("expected command and model input fields")
    );
    assert!(
        requests.is_empty(),
        "invalid input must not execute effects"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn output_codec_refuses_inherited_required_fields() {
    let (result, requests) = sdk_script(
        r#"
Object.prototype.session = "12345678-1234-1234-1234-123456789abc";
const output = sdk.jsonCodec(value => {
  if (typeof value !== "object" || value === null || !("session" in value)
    || typeof value.session !== "string") throw new TypeError("expected own session result");
  return { session: value.session };
});
output.encode({ session: "22345678-1234-1234-1234-123456789abc" });
output.encode({});
"#,
        [],
    )
    .await;
    assert!(
        result
            .expect_err("inherited fields cannot supply a result")
            .to_string()
            .contains("expected own session result")
    );
    assert!(requests.is_empty());
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
async fn sdk_preserves_payloads_when_intrinsics_are_replaced() {
    let (result, requests) = sdk_script(
        r#"
const descriptor = Object.getOwnPropertyDescriptor;
const define = Object.defineProperty;
const remove = Reflect.deleteProperty;
const fromCode = String.fromCharCode;
const TestError = Error;
const nativeApply = Reflect.apply;
let tamperedRequests = 0;
Reflect.defineProperty(Function.prototype, "call", {
  value: function (receiver, ...args) {
    const request = args[1];
    if (request !== null && typeof request === "object" && typeof request.kind === "string") {
      tamperedRequests++;
      request.payload = [1];
      if (request.kind === "effect") {
        request.capability = "blob";
        request.method = "tampered";
      }
    }
    return nativeApply(this, receiver, args);
  },
});
const typedArrayPrototype = Object.getPrototypeOf(Uint8Array.prototype);
const byteLength = Function.prototype.call.bind(descriptor(typedArrayPrototype, "length").get);
const input = new Uint8Array([34, 233, 155, 170, 240, 159, 152, 128, 34]);
const raw = new Uint8Array([0, 255]);
const identity = "12345678-1234-1234-1234-123456789abc";
const invalidCalls = [
  () => sdk.session.create({ command: "invalid", model: identity }),
  () => sdk.session.turn({ command: identity, session: identity, text: "hello", defaults_version: "0" }),
  () => sdk.session.turn({ command: identity, session: identity, text: "\u0000", defaults_version: "1" }),
  () => sdk.register({ id: identity, name: "example", revision: "revision", source: [0], artifact: "export {};", grants: ["invalid"] }),
];
const invalidInputs = [new Uint8Array([255]), new Uint8Array([123]), new Uint16Array([49]), { 0: 49, length: 1 }];
const cyclic = {}; cyclic.self = cyclic;
const invalid = [NaN, -0, { missing: undefined }, [undefined], new Array(1), cyclic];
const targets = [
  [JSON, "parse"], [JSON, "stringify"],
  [globalThis, "TextEncoder"], [globalThis, "TextDecoder"],
  [globalThis, "decodeURIComponent"], [Uint8Array, "from"],
  [Uint8Array, Symbol.hasInstance],
  [typedArrayPrototype, "length"], [typedArrayPrototype, Symbol.toStringTag],
  [typedArrayPrototype, Symbol.iterator], [typedArrayPrototype, "set"], [typedArrayPrototype, "subarray"],
  [String.prototype, "replace"], [String.prototype, "charCodeAt"],
  [String.prototype, "padStart"], [String.prototype, Symbol.iterator],
  [String.prototype, "includes"], [String.prototype, "isWellFormed"],
  [RegExp.prototype, "test"], [RegExp.prototype, "exec"],
  [Number.prototype, "toString"], [Number, "isFinite"], [Number, "isInteger"],
  [Array, "from"], [Array, "isArray"], [Array.prototype, "join"],
  [Array.prototype, "every"], [Array.prototype, "includes"], [Array.prototype, Symbol.iterator],
  [Object.prototype, "toJSON"], [Array.prototype, "toJSON"],
  [Object, "freeze"], [Object, "create"], [Object, "defineProperty"],
  [Object, "getPrototypeOf"], [Object, "setPrototypeOf"],
  [Object, "getOwnPropertyDescriptor"], [Object, "hasOwn"], [Object, "is"], [Object, "keys"],
  [Reflect, "ownKeys"], [Reflect, "apply"], [Set.prototype, "has"], [Set.prototype, "add"],
  [Set.prototype, "delete"], [Function.prototype, "apply"], [Function.prototype, "bind"],
  [globalThis, "JSON"], [globalThis, "Uint8Array"], [globalThis, "Object"],
  [globalThis, "Array"], [globalThis, "String"], [globalThis, "Number"],
  [globalThis, "Set"], [globalThis, "TypeError"], [globalThis, "BigInt"], [globalThis, "RegExp"],
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
  for (let index = 0; index < invalidCalls.length; index++) {
    let rejected = false;
    try { invalidCalls[index](); } catch { rejected = true; }
    if (!rejected) throw new TestError("replaced globals bypassed wrapper validation for case " + index);
  }
  const turn = await sdk.session.turn({ command: identity, session: identity,
    text: "雪😀", defaults_version: "18446744073709551615" });
  if (turn.kind !== "answer" || turn.value.outcome !== "completed" || turn.value.digest[31] !== 255) {
    throw new TestError("replaced globals changed the typed turn answer: " + turn.kind + "/" + turn.value?.outcome + "/" + turn.value?.digest?.[31]);
  }
  turn.value.digest.push(1);
  if (turn.value.digest.length !== 33) throw new TestError("the typed digest must remain an ordinary mutable array");
  const created = await sdk.session.create({ command: identity, model: identity });
  if (created.kind !== "answer" || created.value.session !== identity) throw new TestError("wrong session answer");
  const registered = await sdk.register({ id: identity, name: "example", revision: "revision",
    source: [0, 255], artifact: "export {}; // 雪", grants: ["session"] });
  if (registered.kind !== "answer" || registered.value.registration !== identity) throw new TestError("wrong registration answer");
  await sdk.effect("time", "sample", raw);
  await sdk.now(raw);
  await sdk.random(raw);
  await sdk.sleep(raw);
  await sdk.awaitEvent(raw);
  if (tamperedRequests !== 0) throw new TestError("replaced call intercepted native request dispatch");
} finally {
  for (let index = 0; index < targets.length; index++) {
    if (saved[index] === undefined) remove(targets[index][0], targets[index][1]);
    else define(targets[index][0], targets[index][1], saved[index]);
  }
}
"#,
        [
            DeliveryKind::Answer { resolves: SCRIPTED_REQUEST, payload: InlineFramePayload::new(serde_json::to_vec(&serde_json::json!({
                "session": "12345678-1234-1234-1234-123456789abc",
                "turn": "12345678-1234-1234-1234-123456789abc",
                "accepted_input": "12345678-1234-1234-1234-123456789abc",
                "digest": vec![255_u8; 32], "outcome": "completed"
            })).expect("valid turn answer")) },
            DeliveryKind::Answer { resolves: SCRIPTED_REQUEST, payload: InlineFramePayload::new(br#"{"session":"12345678-1234-1234-1234-123456789abc"}"#.as_slice()) },
            DeliveryKind::Answer { resolves: SCRIPTED_REQUEST, payload: InlineFramePayload::new(br#"{"registration":"12345678-1234-1234-1234-123456789abc"}"#.as_slice()) },
            DeliveryKind::Answer { resolves: SCRIPTED_REQUEST, payload: InlineFramePayload::default() },
            DeliveryKind::Answer { resolves: SCRIPTED_REQUEST, payload: InlineFramePayload::default() },
            DeliveryKind::Answer { resolves: SCRIPTED_REQUEST, payload: InlineFramePayload::default() },
            DeliveryKind::Wake { resolves: SCRIPTED_REQUEST, payload: InlineFramePayload::default() },
            DeliveryKind::Wake { resolves: SCRIPTED_REQUEST, payload: InlineFramePayload::default() },
        ],
    )
    .await;
    result.expect(
        "simultaneous intrinsic replacement must preserve SDK payload encoding, decoding and rejection",
    );
    assert_eq!(requests.len(), 8);
    for (index, method, expected) in [
        (
            0,
            "turn",
            serde_json::json!({
                "command": "12345678-1234-1234-1234-123456789abc",
                "session": "12345678-1234-1234-1234-123456789abc",
                "text": "雪😀", "defaults_version": u64::MAX
            }),
        ),
        (
            1,
            "create",
            serde_json::json!({
                "command": "12345678-1234-1234-1234-123456789abc",
                "model": "12345678-1234-1234-1234-123456789abc"
            }),
        ),
        (
            2,
            "register",
            serde_json::json!({
                "id": "12345678-1234-1234-1234-123456789abc", "name": "example",
                "revision": "revision", "source": [0, 255], "artifact": "export {}; // 雪", "grants": ["session"]
            }),
        ),
    ] {
        let RequestKind::Effect(request) = &requests[index] else {
            panic!("expected {method} effect")
        };
        assert_eq!(request.method(), method);
        let actual: serde_json::Value =
            serde_json::from_slice(request.payload().as_bytes()).expect("valid effect JSON");
        assert_eq!(
            actual, expected,
            "intrinsic replacement changed the {method} payload"
        );
    }
    let raw = InlineFramePayload::new(vec![0, 255]);
    assert_eq!(
        &requests[3..],
        &[
            RequestKind::Effect(EffectRequest::new(
                signalbox_domain::ProgramCapability::Time,
                "sample".into(),
                raw.clone()
            )),
            RequestKind::Now(raw.clone()),
            RequestKind::Random(raw.clone()),
            RequestKind::Sleep(raw.clone()),
            RequestKind::AwaitEvent(raw),
        ]
    );
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
async fn answer_wrappers_refuse_extra_success_fields() {
    const IDENTITY: &str = "12345678-1234-1234-1234-123456789abc";
    let turn_answer = serde_json::json!({ "session": IDENTITY, "turn": IDENTITY,
        "accepted_input": IDENTITY, "digest": vec![0; 32], "outcome": "completed" });
    let mut extended_turn_answer = turn_answer.clone();
    extended_turn_answer["unexpected"] = serde_json::json!(true);
    for (call, payload) in [
        (
            "sdk.session.create({ command: identity, model: identity })",
            turn_answer,
        ),
        (
            "sdk.register({ id: identity, name: 'example', revision: 'revision', source: [], artifact: 'export {};', grants: [] })",
            serde_json::json!({ "registration": IDENTITY, "unexpected": true }),
        ),
        (
            "sdk.session.turn({ command: identity, session: identity, text: 'hello', defaults_version: '1' })",
            extended_turn_answer,
        ),
    ] {
        let (result, requests) = sdk_script(
            &format!("const identity = {IDENTITY:?}; await {call};"),
            [DeliveryKind::Answer {
                resolves: SCRIPTED_REQUEST,
                payload: InlineFramePayload::new(
                    serde_json::to_vec(&payload).expect("JSON answer fixture"),
                ),
            }],
        )
        .await;
        assert!(
            result.is_err(),
            "{call} must refuse unexpected success fields in {payload}"
        );
        assert_eq!(requests.len(), 1, "{call} must reach answer validation");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn answer_wrappers_refuse_inherited_fields() {
    for (fields, call) in [
        (
            "{ outcome: 'refused' }",
            "sdk.session.create({ command: identity, model: identity })",
        ),
        ("{ outcome: 'ambiguous' }", "sdk.register(registration)"),
        (
            "{ session: identity }",
            "sdk.session.create({ command: identity, model: identity })",
        ),
        ("{ registration: identity }", "sdk.register(registration)"),
        (
            "{ outcome: 'completed', session: identity, turn: identity, accepted_input: identity, digest: new Array(32).fill(0) }",
            "sdk.session.turn({ command: identity, session: identity, text: 'hello', defaults_version: '1' })",
        ),
    ] {
        let (result, requests) = sdk_script(
            &format!(
                r#"
const identity = "12345678-1234-1234-1234-123456789abc";
const registration = {{ id: identity, name: "example", revision: "revision",
  source: [], artifact: "export {{}};", grants: [] }};
Object.assign(Object.prototype, {fields});
await {call};
"#
            ),
            [DeliveryKind::Answer {
                resolves: SCRIPTED_REQUEST,
                payload: InlineFramePayload::new(br#"{"unexpected":1}"#.as_slice()),
            }],
        )
        .await;
        assert!(
            result.is_err(),
            "{call} must not accept inherited fields {fields} as answer data"
        );
        assert_eq!(
            requests.len(),
            1,
            "the malformed answer must fail after the {call} request"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn answer_validation_ignores_inherited_accessors() {
    let (result, requests) = sdk_script(
        r#"
const identity = "12345678-1234-1234-1234-123456789abc";
Object.defineProperty(Object.prototype, "outcome", {
  get() { throw new Error("answer validation invoked an inherited accessor"); }
});
const answer = await sdk.session.create({ command: identity, model: identity });
if (answer.kind !== "answer" || answer.value.session !== identity) {
  throw new Error("own answer data must remain valid despite inherited accessors");
}
"#,
        [DeliveryKind::Answer {
            resolves: SCRIPTED_REQUEST,
            payload: InlineFramePayload::new(
                br#"{"session":"12345678-1234-1234-1234-123456789abc"}"#.as_slice(),
            ),
        }],
    )
    .await;
    result.expect("answer inspection must read only its own data properties");
    assert_eq!(requests.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn session_wrapper_refuses_malformed_host_answer() {
    let (result, requests) = sdk_script(
        r#"
const identity = "12345678-1234-1234-1234-123456789abc";
await sdk.session.create({ command: identity, model: identity });
"#,
        [DeliveryKind::Answer {
            resolves: SCRIPTED_REQUEST,
            payload: InlineFramePayload::new(br#"{"session":17}"#.as_slice()),
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
        [DeliveryKind::Answer { resolves: SCRIPTED_REQUEST, payload: InlineFramePayload::new(br#"{"outcome":"refused"}"#.as_slice()) }],
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
        [DeliveryKind::Reject { resolves: SCRIPTED_REQUEST, reason: RejectReason::CapabilityDenied }],
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

#[tokio::test(flavor = "current_thread")]
async fn typed_primitives_preserve_full_width_values_and_exact_event_bytes()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::program_primitives::{
        AwaitProgramEvent, ProgramEvent, ProgramEventSource, RandomValue, SleepUntil, UnixMillis,
    };
    const SOURCE_RUN: &str = "12345678-1234-1234-1234-123456789abc";
    let now = UnixMillis(9_007_199_254_740_993);
    let random = RandomValue(u64::MAX);
    let deadline = SleepUntil(UnixMillis(9_007_199_254_740_994));
    let event = ProgramEvent {
        position: u64::MAX,
        payload: InlineFramePayload::new(vec![0, 128, 255]),
    };
    let (result, requests) = sdk_script(
        r#"
const now = await sdk.primitives.now();
const random = await sdk.primitives.random();
const wake = await sdk.primitives.sleepUntil("9007199254740994");
const event = await sdk.primitives.awaitEvent({ source: { kind: "program_answers", run: "12345678-1234-1234-1234-123456789abc" }, after: "9007199254740993" });
if (now.value !== "9007199254740993" || random.value !== "18446744073709551615" || wake.value !== "9007199254740994" || event.value.position !== "18446744073709551615" || event.value.payload.join() !== "0,128,255") throw new Error("primitive precision lost");
"#,
        [
            DeliveryKind::Answer { resolves: SCRIPTED_REQUEST, payload: now.encode() },
            DeliveryKind::Answer { resolves: SCRIPTED_REQUEST, payload: random.encode() },
            DeliveryKind::Wake { resolves: SCRIPTED_REQUEST, payload: deadline.0.encode() },
            DeliveryKind::Answer { resolves: SCRIPTED_REQUEST, payload: event.encode() },
        ],
    ).await;
    result?;
    let wait = AwaitProgramEvent {
        source: ProgramEventSource::ProgramAnswers(ProgramRunId::from_uuid(uuid::Uuid::parse_str(
            SOURCE_RUN,
        )?)),
        after: now.0,
    };
    assert_eq!(
        requests,
        vec![
            RequestKind::Now(InlineFramePayload::default()),
            RequestKind::Random(InlineFramePayload::default()),
            RequestKind::Sleep(deadline.encode()),
            RequestKind::AwaitEvent(wait.encode())
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn evaluation_codec_refuses_invalid_trial_identity_before_request() {
    let (result, requests) = sdk_script("await sdk.evaluation.judge({ trial: -1 });", []).await;
    assert!(result.is_err());
    assert!(requests.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn evaluation_codec_refuses_invalid_blob_bytes() {
    let (result, requests) = sdk_script(
        "await sdk.evaluation.blob({ digest: 'sha256:' + 'a'.repeat(64) });",
        [DeliveryKind::Answer {
            resolves: SCRIPTED_REQUEST,
            payload: InlineFramePayload::new(br#"{"bytes":[256]}"#.as_slice()),
        }],
    )
    .await;
    assert!(result.is_err());
    assert_eq!(requests.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn evaluation_judge_codec_preserves_full_width_usage() {
    let answer = serde_json::json!({
        "outcome": "verdict", "call": "12345678-1234-1234-1234-123456789abc",
        "request_digest": format!("sha256:{}", "a".repeat(64)),
        "binding": { "selection": "12345678-1234-1234-1234-123456789abc", "target": "12345678-1234-1234-1234-123456789abc", "credential_reference": "fixture", "provider_model": "fixture", "contract_digest": "fixture", "cache_accounting": "input_excludes_cache" },
        "actual": "approve", "rationale": "Recorded decision.", "provider_reported_model": null,
        "usage": { "input_tokens": "18446744073709551615", "output_tokens": "0", "cache_creation_input_tokens": null, "cache_read_input_tokens": "9007199254740993" }
    });
    let (result, requests) = sdk_script(
        r#"const result = await sdk.evaluation.judge({ trial: 0 });
        if (result.kind !== 'answer' || result.value.usage.input_tokens !== '18446744073709551615' || result.value.usage.cache_read_input_tokens !== '9007199254740993') throw new Error('usage rounded');"#,
        [DeliveryKind::Answer { resolves: SCRIPTED_REQUEST, payload: InlineFramePayload::new(serde_json::to_vec(&answer).unwrap()) }],
    ).await;
    result.unwrap();
    assert_eq!(requests.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn evaluation_blob_codec_refuses_trailing_digest_bytes_before_request() {
    let (result, requests) = sdk_script(
        "await sdk.evaluation.blob({ digest: 'sha256:' + 'a'.repeat(64) + '\\n' });",
        [],
    )
    .await;
    assert!(result.is_err());
    assert!(requests.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn evaluation_failed_trial_codec_retains_the_reported_model() {
    let answer = serde_json::json!({
        "outcome": "failed", "call": "12345678-1234-1234-1234-123456789abc",
        "request_digest": format!("sha256:{}", "a".repeat(64)),
        "binding": { "selection": "12345678-1234-1234-1234-123456789abc", "target": "12345678-1234-1234-1234-123456789abc", "credential_reference": "fixture", "provider_model": "configured-model", "contract_digest": "fixture", "cache_accounting": "input_excludes_cache" },
        "cause": "provider_target_substituted", "provider_reported_model": "substituted-model",
        "usage": { "input_tokens": "80", "output_tokens": "20", "cache_creation_input_tokens": null, "cache_read_input_tokens": null }
    });
    let (result, requests) = sdk_script(
        r#"const result = await sdk.evaluation.judge({ trial: 0 });
        if (result.kind !== 'answer' || result.value.cause !== 'provider_target_substituted' || result.value.provider_reported_model !== 'substituted-model') throw new Error('model evidence lost');"#,
        [DeliveryKind::Answer { resolves: SCRIPTED_REQUEST, payload: InlineFramePayload::new(serde_json::to_vec(&answer).unwrap()) }],
    ).await;
    result.unwrap();
    assert_eq!(requests.len(), 1);
}
