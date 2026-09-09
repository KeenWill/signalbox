// The host creates input and empty result bytes during preload, before program code.
const empty = new Uint8Array();

export default async function invoke(program) {
  if (!("default" in program)) return empty;
  const run = program.default;
  return await run(input);
}
