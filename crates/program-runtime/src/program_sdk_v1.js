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
