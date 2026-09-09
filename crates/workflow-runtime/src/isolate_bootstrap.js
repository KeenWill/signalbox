Object.defineProperty(globalThis, "__signalboxProgramRequest", {
  value: Deno.core.ops.op_program_request,
  writable: false,
  enumerable: false,
  configurable: true,
});
Reflect.deleteProperty(globalThis, "Deno");
Object.defineProperty(Math, "random", {
  value: undefined,
  writable: false,
  configurable: false,
});
Object.defineProperty(globalThis, "Date", {
  value: undefined,
  writable: false,
  configurable: false,
});
Object.defineProperty(globalThis, "Intl", {
  value: undefined,
  writable: false,
  configurable: false,
});
// `Temporal` is a second ambient clock, independent of `Date`:
// `Temporal.Now.instant()` reads the host clock and `Temporal.Now.timeZoneId()`
// reads its zone. Either can reach an SDK payload and make the next durable
// request depend on when and where the attempt ran.
Object.defineProperty(globalThis, "Temporal", {
  value: undefined,
  writable: false,
  configurable: false,
});
Object.defineProperty(globalThis, "WeakRef", {
  value: undefined,
  writable: false,
  configurable: false,
});
Object.defineProperty(globalThis, "FinalizationRegistry", {
  value: undefined,
  writable: false,
  configurable: false,
});
// `Atomics.waitAsync` settles a promise on a wall-clock timeout and
// `Atomics.wait` blocks the isolate thread outright, so racing either against a
// journaled request can pick a different next request on replay, or wedge the
// thread before the host can report `Stalled`. Both need a `SharedArrayBuffer`,
// and neither the buffer nor the rest of `Atomics` has a use in an isolate that
// never shares memory with another thread.
Object.defineProperty(globalThis, "SharedArrayBuffer", {
  value: undefined,
  writable: false,
  configurable: false,
});
Object.defineProperty(globalThis, "Atomics", {
  value: undefined,
  writable: false,
  configurable: false,
});
// Removing those two bindings does not close the wait path on its own:
// `new WebAssembly.Memory({shared: true})` still returns a real
// `SharedArrayBuffer`, and WebAssembly's own `memory.atomic.wait32` can then
// block the isolate thread with no timeout, before the event loop can report
// `Stalled`. The artifact contract is JavaScript, so the whole namespace goes
// rather than only its shared-memory corner.
Object.defineProperty(globalThis, "WebAssembly", {
  value: undefined,
  writable: false,
  configurable: false,
});
// Deleting `Intl` leaves the Intl-backed prototype methods: `toLocaleString`,
// `localeCompare`, and the locale-aware case mappings still read the host's
// default locale and ICU data. An artifact that encodes one of those results
// into a request diverges the moment it restarts on a host configured
// differently, so the closed isolate removes them rather than pinning a locale
// an artifact could not observe it had been given.
// The bootstrap runs as a classic script, so a top-level `const` here would
// become a global lexical binding the artifact could read. Nothing this file
// needs to name belongs to the isolate it hands over.
(() => {
  const typedArrayPrototype = Object.getPrototypeOf(Int8Array.prototype);
  const localeSensitiveMethods = [
    [Object.prototype, "toLocaleString"],
    [Number.prototype, "toLocaleString"],
    [BigInt.prototype, "toLocaleString"],
    [Array.prototype, "toLocaleString"],
    [typedArrayPrototype, "toLocaleString"],
    [String.prototype, "localeCompare"],
    [String.prototype, "toLocaleLowerCase"],
    [String.prototype, "toLocaleUpperCase"],
  ];
  for (const [holder, name] of localeSensitiveMethods) {
    Object.defineProperty(holder, name, {
      value: undefined,
      writable: false,
      configurable: false,
    });
  }
})();
