(() => {
  const request = globalThis.__signalboxProgramRequest;
  Reflect.deleteProperty(globalThis, "__signalboxProgramRequest");

  const call = (kind, payload) => {
    if (!(payload instanceof Uint8Array)) {
      throw new TypeError("program frame payload must be a Uint8Array");
    }
    return request({ kind, payload: Array.from(payload) });
  };

  return Object.freeze({
    effect(capability, method, payload) {
      if (!(payload instanceof Uint8Array)) {
        throw new TypeError("program frame payload must be a Uint8Array");
      }
      return request({ kind: "effect", capability, method, payload: Array.from(payload) });
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
