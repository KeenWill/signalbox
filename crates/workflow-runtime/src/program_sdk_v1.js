(() => {
  const request = globalThis.__signalboxProgramRequest;
  Reflect.deleteProperty(globalThis, "__signalboxProgramRequest");
  const { stringify: jsonStringify, parse: jsonParse } = JSON;
  const decodeUriComponent = decodeURIComponent;
  const { defineProperty, freeze, getOwnPropertyDescriptor, getPrototypeOf,
    setPrototypeOf, hasOwn, is: sameValue } = Object;
  const objectPrototype = Object.prototype;
  const arrayPrototype = Array.prototype;
  const isArray = Array.isArray;
  const ownKeys = Reflect.ownKeys;
  const { isFinite, isInteger } = Number;
  const ByteArray = Uint8Array;
  const CodecTypeError = TypeError;
  const charCodeAt = Function.prototype.call.bind(String.prototype.charCodeAt);
  const typedArrayPrototype = getPrototypeOf(ByteArray.prototype);
  const typedArrayKind = Function.prototype.call.bind(
    getOwnPropertyDescriptor(typedArrayPrototype, Symbol.toStringTag).get);
  const typedArrayLength = Function.prototype.call.bind(
    getOwnPropertyDescriptor(typedArrayPrototype, "length").get);

  const call = (kind, payload) => {
    if (!(payload instanceof Uint8Array)) {
      throw new TypeError("program frame payload must be a Uint8Array");
    }
    return request({ kind, payload: Array.from(payload) });
  };

  const bytes = (value) => {
    if (typedArrayKind(value) !== "Uint8Array") {
      throw new CodecTypeError("program codec must produce a Uint8Array");
    }
    return value;
  };
  const record = (value) => {
    if (value === null || typeof value !== "object" || Array.isArray(value)) {
      throw new TypeError("expected a program record");
    }
    return value;
  };
  const string = (value) => {
    if (typeof value !== "string" || !value.isWellFormed()) throw new TypeError("expected a Unicode string");
    return value;
  };
  const uuid = (value) => {
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(string(value))) {
      throw new TypeError("expected a UUID");
    }
    return value;
  };
  const byteArray = (value) => {
    if (!Array.isArray(value) || !Array.from(value).every((byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255)) {
      throw new TypeError("expected bytes");
    }
    return value;
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
    decode(value) { return decode(decodeJson(value)); },
    encode(value) { return encodeJson(decode(value)); },
  });
  const effect = (capability, method, payload) => request({
    kind: "effect", capability, method, payload: Array.from(bytes(payload)),
  });
  const answer = async (delivery, decode) => {
    const result = await delivery;
    if (result.kind !== "answer") return result;
    return { kind: "answer", value: decode(record(decodeJson(new Uint8Array(result.payload)))) };
  };
  const refusedOrAmbiguous = (value) =>
    Object.keys(value).length === 1 && ["refused", "ambiguous"].includes(value.outcome);
  const session = Object.freeze({
    create(input) {
      record(input);
      const payload = encodeJson({ command: uuid(input.command), model: uuid(input.model) });
      return answer(effect("session", "create", payload), (value) => {
        if (refusedOrAmbiguous(value)) return value;
        return { session: uuid(value.session) };
      });
    },
    turn(input) {
      record(input);
      const version = string(input.defaults_version);
      if (!/^[1-9][0-9]*$/.test(version) || BigInt(version) > 18446744073709551615n) {
        throw new TypeError("expected a positive u64 decimal string");
      }
      const text = string(input.text);
      if (text.length === 0 || text.includes("\u0000")) throw new TypeError("expected nonempty session text without NUL");
      const fields = encodeJson({ command: uuid(input.command), session: uuid(input.session), text });
      // The Rust wire record uses a JSON u64; write its decimal digits directly.
      const suffix = Uint8Array.from(',"defaults_version":' + version + '}', (character) => character.charCodeAt(0));
      const payload = new Uint8Array(fields.length - 1 + suffix.length);
      payload.set(fields.subarray(0, -1));
      payload.set(suffix, fields.length - 1);
      return answer(effect("session", "turn", payload), (value) => {
        if (refusedOrAmbiguous(value)) return value;
        if (!["completed", "refused", "failed", "cancelled", "retired", "ambiguous"].includes(value.outcome)) {
          throw new TypeError("invalid session disposition");
        }
        const digest = byteArray(value.digest);
        if (digest.length !== 32) throw new TypeError("expected a SHA-256 digest");
        return { session: uuid(value.session), turn: uuid(value.turn),
          accepted_input: uuid(value.accepted_input), digest, outcome: value.outcome };
      });
    },
  });
  const capabilities = ["time", "random", "sleep", "subscribe", "session", "judge",
    "exec-stage", "corpus", "eval-record", "blob", "register"];

  return Object.freeze({
    jsonCodec,
    defineProgram({ input, output, run }) {
      return async (payload) => bytes(output.encode(await run(input.decode(bytes(payload)))));
    },
    session,
    register(input) {
      record(input);
      if (!Array.isArray(input.grants) || !Array.from(input.grants).every((grant) => capabilities.includes(grant))) {
        throw new TypeError("invalid program grants");
      }
      const payload = encodeJson({ id: uuid(input.id), name: string(input.name),
        revision: string(input.revision), source: byteArray(input.source),
        artifact: string(input.artifact), grants: input.grants });
      return answer(effect("register", "register", payload), (value) => {
        if (value.outcome === "ambiguous" && Object.keys(value).length === 1) return value;
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
