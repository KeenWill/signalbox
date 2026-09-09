(() => {
  const request = globalThis.__signalboxProgramRequest;
  Reflect.deleteProperty(globalThis, "__signalboxProgramRequest");
  const { stringify: jsonStringify, parse: jsonParse } = JSON;
  const decodeUriComponent = decodeURIComponent;
  const { defineProperty, freeze, getOwnPropertyDescriptor, getPrototypeOf,
    setPrototypeOf, hasOwn, keys: objectKeys, is: sameValue } = Object;
  const objectPrototype = Object.prototype;
  const arrayPrototype = Array.prototype;
  const isArray = Array.isArray;
  const arrayFrom = Array.from.bind(Array);
  const ownKeys = Reflect.ownKeys;
  const { isFinite, isInteger } = Number;
  const ByteArray = Uint8Array;
  const CodecTypeError = TypeError;
  const parseBigInt = BigInt;
  const charCodeAt = Function.prototype.call.bind(String.prototype.charCodeAt);
  const isWellFormed = Function.prototype.call.bind(String.prototype.isWellFormed);
  const stringIncludes = Function.prototype.call.bind(String.prototype.includes);
  const regexExec = Function.prototype.call.bind(RegExp.prototype.exec);
  const typedArrayPrototype = getPrototypeOf(ByteArray.prototype);
  const typedArrayKind = Function.prototype.call.bind(
    getOwnPropertyDescriptor(typedArrayPrototype, Symbol.toStringTag).get);
  const typedArrayLength = Function.prototype.call.bind(
    getOwnPropertyDescriptor(typedArrayPrototype, "length").get);

  const call = (kind, payload) => request({ kind, payload: wireBytes(payload) });

  const bytes = (value) => {
    if (typedArrayKind(value) !== "Uint8Array") {
      throw new CodecTypeError("program codec must produce a Uint8Array");
    }
    return value;
  };
  const wireBytes = (value) => {
    const payload = bytes(value);
    const result = setPrototypeOf([], null);
    for (let index = 0; index < typedArrayLength(payload); index++) result[index] = payload[index];
    return result;
  };
  const contains = (values, value) => {
    for (let index = 0; index < values.length; index++) {
      if (values[index] === value) return true;
    }
    return false;
  };
  const record = (value) => {
    if (value === null || typeof value !== "object" || isArray(value)) {
      throw new CodecTypeError("expected a program record");
    }
    return value;
  };
  const string = (value) => {
    if (typeof value !== "string" || !isWellFormed(value)) throw new CodecTypeError("expected a Unicode string");
    return value;
  };
  const uuid = (value) => {
    if (regexExec(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i, string(value)) === null) {
      throw new CodecTypeError("expected a UUID");
    }
    return value;
  };
  const byteArray = (value) => {
    if (!isArray(value)) throw new CodecTypeError("expected bytes");
    const result = snapshotJson(value);
    for (let index = 0; index < result.length; index++) {
      const byte = result[index];
      if (!isInteger(byte) || byte < 0 || byte > 255) throw new CodecTypeError("expected bytes");
    }
    return arrayFrom(result);
  };
  const snapshotJson = (value, ancestors = null) => {
    if (value === null || typeof value === "string" || typeof value === "boolean") return value;
    if (typeof value === "number" && isFinite(value) && !sameValue(value, -0)) return value;
    if (typeof value !== "object") throw new CodecTypeError("expected a lossless JSON value");
    for (let ancestor = ancestors; ancestor !== null; ancestor = ancestor.parent) {
      if (ancestor.value === value) throw new CodecTypeError("expected acyclic JSON data");
    }
    const array = isArray(value);
    const prototype = getPrototypeOf(value);
    if (prototype !== (array ? arrayPrototype : objectPrototype) && prototype !== null) {
      throw new CodecTypeError("expected a JSON record or array");
    }
    const keys = ownKeys(value);
    const length = array ? getOwnPropertyDescriptor(value, "length").value : 0;
    if (array) {
      if (!isInteger(length) || length < 0 || keys.length !== length + 1) {
        throw new CodecTypeError("expected a dense JSON array without extra properties");
      }
    }
    const snapshot = array ? setPrototypeOf([], null) : { __proto__: null };
    const path = { __proto__: null, value, parent: ancestors };
    for (let index = 0; index < keys.length; index++) {
      const key = keys[index];
      if (array && key === "length") continue;
      const property = getOwnPropertyDescriptor(value, key);
      if (typeof key !== "string" || property === undefined || !property.enumerable || !hasOwn(property, "value")) {
        throw new CodecTypeError("expected an enumerable JSON data property");
      }
      defineProperty(snapshot, key, { __proto__: null,
        value: snapshotJson(property.value, path), enumerable: true });
    }
    if (array) {
      for (let index = 0; index < length; index++) {
        if (!hasOwn(snapshot, index)) throw new CodecTypeError("expected a dense JSON array");
      }
    }
    return freeze(snapshot);
  };
  const hex = "0123456789abcdef";
  const encodeJson = (value) => {
    const text = jsonStringify(snapshotJson(value));
    // ASCII JSON preserves UTF-16 strings without requiring ambient text codecs.
    let ascii = "";
    for (let index = 0; index < text.length; index++) {
      const code = charCodeAt(text, index);
      ascii += code < 0x7f ? text[index]
        : "\\u" + hex[code >>> 12] + hex[(code >>> 8) & 15] + hex[(code >>> 4) & 15] + hex[code & 15];
    }
    const payload = new ByteArray(ascii.length);
    for (let index = 0; index < ascii.length; index++) payload[index] = charCodeAt(ascii, index);
    return payload;
  };
  const decodeJson = (value) => {
    const payload = bytes(value);
    const length = typedArrayLength(payload);
    let escaped = "";
    for (let index = 0; index < length; index++) {
      const byte = payload[index];
      escaped += "%" + hex[byte >>> 4] + hex[byte & 15];
    }
    return jsonParse(decodeUriComponent(escaped));
  };
  const jsonCodec = (decode) => freeze({
    decode(value) { return decode(snapshotJson(decodeJson(value))); },
    encode(value) { return encodeJson(decode(snapshotJson(value))); },
  });
  const effect = (capability, method, payload) => request({
    kind: "effect", capability, method, payload: wireBytes(payload),
  });
  const answer = async (delivery, decode) => {
    const result = await delivery;
    if (result.kind !== "answer") return result;
    const data = byteArray(result.payload);
    const payload = new ByteArray(data.length);
    for (let index = 0; index < data.length; index++) payload[index] = data[index];
    return { kind: "answer", value: decode(record(snapshotJson(decodeJson(payload)))) };
  };
  const refusedOrAmbiguous = (value) =>
    objectKeys(value).length === 1 && contains(["refused", "ambiguous"], value.outcome);
  const session = freeze({
    create(input) {
      record(input);
      const payload = encodeJson({ command: uuid(input.command), model: uuid(input.model) });
      return answer(effect("session", "create", payload), (value) => {
        if (refusedOrAmbiguous(value)) return { outcome: value.outcome };
        if (objectKeys(value).length !== 1) throw new CodecTypeError("invalid session creation answer fields");
        return { session: uuid(value.session) };
      });
    },
    turn(input) {
      record(input);
      const version = string(input.defaults_version);
      if (regexExec(/^[1-9][0-9]*$/, version) === null || parseBigInt(version) > 18446744073709551615n) {
        throw new CodecTypeError("expected a positive u64 decimal string");
      }
      const text = string(input.text);
      if (text.length === 0 || stringIncludes(text, "\u0000")) throw new CodecTypeError("expected nonempty session text without NUL");
      const fields = encodeJson({ command: uuid(input.command), session: uuid(input.session), text });
      // The Rust wire record uses a JSON u64; write its decimal digits directly.
      const suffix = ',"defaults_version":' + version + '}';
      const prefixLength = typedArrayLength(fields) - 1;
      const payload = new ByteArray(prefixLength + suffix.length);
      for (let index = 0; index < prefixLength; index++) payload[index] = fields[index];
      for (let index = 0; index < suffix.length; index++) payload[prefixLength + index] = charCodeAt(suffix, index);
      return answer(effect("session", "turn", payload), (value) => {
        if (refusedOrAmbiguous(value)) return { outcome: value.outcome };
        if (objectKeys(value).length !== 5) throw new CodecTypeError("invalid session turn answer fields");
        if (!contains(["completed", "refused", "failed", "cancelled", "retired", "ambiguous"], value.outcome)) {
          throw new CodecTypeError("invalid session disposition");
        }
        const digest = byteArray(value.digest);
        if (digest.length !== 32) throw new CodecTypeError("expected a SHA-256 digest");
        return { session: uuid(value.session), turn: uuid(value.turn),
          accepted_input: uuid(value.accepted_input), digest, outcome: value.outcome };
      });
    },
  });
  const capabilities = ["time", "random", "sleep", "subscribe", "session", "judge",
    "exec-stage", "corpus", "eval-record", "blob", "register"];

  const unsigned = (value) => {
    if (typeof value !== "string" || !regexExec(/^(0|[1-9][0-9]*)$/, value)
        || parseBigInt(value) > 18446744073709551615n) {
      throw new CodecTypeError("expected a u64 decimal string");
    }
    return value;
  };
  const field = (value, name) => {
    if (objectKeys(value).length !== 1) throw new CodecTypeError("invalid primitive answer fields");
    return unsigned(value[name]);
  };
  const primitives = freeze({
    now() { return answer(call("now", new ByteArray()), (value) => field(value, "unix_ms")); },
    random() { return answer(call("random", new ByteArray()), (value) => field(value, "value")); },
    async sleepUntil(deadlineUnixMs) {
      const result = await call("sleep", encodeJson({ deadline_unix_ms: unsigned(deadlineUnixMs) }));
      if (result.kind !== "wake") return result;
      const data = byteArray(result.payload);
      const payload = new ByteArray(data.length);
      for (let index = 0; index < data.length; index++) payload[index] = data[index];
      return { kind: "wake", value: field(record(snapshotJson(decodeJson(payload))), "unix_ms") };
    },
    awaitEvent(input) {
      record(input);
      record(input.source);
      if (input.source.kind !== "program_answers") throw new CodecTypeError("invalid program event source");
      const payload = encodeJson({ source: { kind: "program_answers", run: uuid(input.source.run) }, after: unsigned(input.after) });
      return answer(call("await_event", payload), (value) => {
        if (objectKeys(value).length !== 2) throw new CodecTypeError("invalid program event fields");
        const position = unsigned(value.position);
        if (position === "0") throw new CodecTypeError("expected a positive event position");
        return { position, payload: byteArray(value.payload) };
      });
    },
  });

  const exact = (value, fields) => {
    record(value);
    if (objectKeys(value).length !== fields.length) throw new CodecTypeError("invalid evaluation fields");
    for (let index = 0; index < fields.length; index++) if (!hasOwn(value, fields[index])) throw new CodecTypeError("missing evaluation field");
    return value;
  };
  const each = (values, check) => {
    for (let index = 0; index < values.length; index++) check(values[index]);
  };
  const nullable = (value, check) => value === null ? null : check(value);
  const disposition = (value) => {
    if (!contains(["approve", "deny", "escalate_to_human"], value)) throw new CodecTypeError("invalid disposition");
    return value;
  };
  const u32 = (value) => {
    if (!isInteger(value) || value < 0 || value > 4294967295) throw new CodecTypeError("expected u32");
    return value;
  };
  const digest = (value) => {
    const text = string(value);
    if (text.length !== 71 || regexExec(/^sha256:[0-9a-f]{64}$/, text) === null) throw new CodecTypeError("invalid blob digest");
    return value;
  };
  const list = (value, check) => {
    if (!isArray(value)) throw new CodecTypeError("expected array");
    for (let index = 0; index < value.length; index++) check(value[index]);
    return value;
  };
  const judgeBinding = (value) => {
    exact(value, ["selection", "target", "credential_reference", "provider_model", "contract_digest", "cache_accounting"]);
    uuid(value.selection); uuid(value.target);
    each(["credential_reference", "provider_model", "contract_digest", "cache_accounting"], (key) => string(value[key]));
    return value;
  };
  const judgeUsage = (value) => {
    exact(value, ["input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"]);
    each(objectKeys(value), (key) => nullable(value[key], unsigned));
    return value;
  };
  const judgeAnswer = (value) => {
    if (value.outcome === "ambiguous") return exact(value, ["outcome"]);
    if (value.outcome === "verdict") {
      exact(value, ["outcome", "call", "request_digest", "binding", "actual", "rationale", "provider_reported_model", "usage"]);
      uuid(value.call); disposition(value.actual); string(value.rationale); nullable(value.provider_reported_model, string);
    } else if (value.outcome === "failed") {
      exact(value, ["outcome", "call", "request_digest", "binding", "cause", "usage"]);
      nullable(value.call, uuid); string(value.cause);
    } else throw new CodecTypeError("invalid judge outcome");
    digest(value.request_digest); judgeBinding(value.binding); judgeUsage(value.usage);
    return value;
  };
  const corpusCase = (value) => {
    exact(value, ["format", "case"]);
    const item = record(value.case);
    if (value.format === "offline") {
      exact(item, ["id", "request", "expected", "label_provenance"]);
      string(item.id); disposition(item.expected); string(item.label_provenance);
      exact(item.request, ["tool", "arguments", "commissioned_goal", "session_template", "frozen_system_prompt"]);
      string(item.request.tool); string(item.request.arguments);
      each(["commissioned_goal", "session_template", "frozen_system_prompt"], (key) => nullable(item.request[key], string));
    } else if (value.format === "live") {
      exact(item, ["name", "category", "tool", "arguments", "expected", "goal", "template", "system_prompt", "dispatch", "notes"]);
      each(["name", "tool", "arguments"], (key) => string(item[key]));
      disposition(item.expected);
      if (!contains(["git_push", "thread_ops", "network_egress", "credential_access", "destructive", "workspace_benign", "injection_resistance", "context_absent", "undecodable_arguments"], item.category)) throw new CodecTypeError("invalid case category");
      each(["goal", "template", "system_prompt", "notes"], (key) => nullable(item[key], string));
      // JSONL pull-request identities are transported as decimal strings.
      nullable(item.dispatch, (fence) => {
        exact(fence, ["repository", "pull_request", "head_sha", "head_repository", "head_branch", "base_branch"]);
        unsigned(fence.pull_request);
        each(["repository", "head_sha", "head_repository", "head_branch", "base_branch"], (key) => string(fence[key]));
      });
    } else throw new CodecTypeError("invalid corpus format");
    return value;
  };
  const evaluation = freeze({
    manifest: jsonCodec((value) => {
      exact(value, ["corpus", "format", "cases", "repeats", "binding", "postures", "speculative_tools"]);
      digest(value.corpus); judgeBinding(value.binding); list(value.cases, u32); u32(value.repeats);
      if (!contains(["offline", "live"], value.format) || value.repeats === 0 || (value.format === "offline" && value.repeats !== 1) || value.cases.length * value.repeats > 1000) throw new CodecTypeError("invalid evaluation trial plan");
      record(value.postures);
      each(objectKeys(value.postures), (key) => string(value.postures[key]));
      list(value.speculative_tools, string);
      return value;
    }),
    corpus() {
      return answer(effect("corpus", "load", encodeJson({})), (value) => {
        exact(value, ["cases", "corpus_digest", "rendered_digest"]);
        list(value.cases, corpusCase); string(value.corpus_digest); string(value.rendered_digest);
        return value;
      });
    },
    judge(input) {
      exact(input, ["trial"]);
      return answer(effect("judge", "evaluate", encodeJson({ trial: u32(input.trial) })), judgeAnswer);
    },
    blob(input) {
      exact(input, ["digest"]);
      return answer(effect("blob", "read", encodeJson({ digest: digest(input.digest) })), (value) => {
        exact(value, ["bytes"]);
        return { bytes: byteArray(value.bytes) };
      });
    },
  });

  return freeze({
    evaluation,
    jsonCodec,
    primitives,
    defineProgram({ input, output, run }) {
      return async (payload) => bytes(output.encode(await run(input.decode(bytes(payload)))));
    },
    session,
    register(input) {
      record(input);
      const grants = snapshotJson(input.grants);
      if (!isArray(grants)) throw new CodecTypeError("invalid program grants");
      for (let index = 0; index < grants.length; index++) {
        if (!contains(capabilities, grants[index])) throw new CodecTypeError("invalid program grants");
      }
      const payload = encodeJson({ id: uuid(input.id), name: string(input.name),
        revision: string(input.revision), source: byteArray(input.source),
        artifact: string(input.artifact), grants });
      return answer(effect("register", "register", payload), (value) => {
        if (value.outcome === "ambiguous" && objectKeys(value).length === 1) return { outcome: value.outcome };
        if (objectKeys(value).length !== 1) throw new CodecTypeError("invalid registration answer fields");
        return { registration: uuid(value.registration) };
      });
    },
    effect(capability, method, payload) {
      return effect(capability, method, payload);
    },
    now(payload) {
      return call("now", payload);
    },
    random(payload) {
      return call("random", payload);
    },
    sleep(payload) {
      return call("sleep", payload);
    },
    awaitEvent(payload) {
      return call("await_event", payload);
    },
  });
})()
