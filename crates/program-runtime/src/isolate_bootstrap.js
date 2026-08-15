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
