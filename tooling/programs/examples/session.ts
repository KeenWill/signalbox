import { defineProgram, jsonCodec, session } from "@signalbox/program-sdk/v1";

interface Input {
  command: string;
  model: string;
}

interface Output {
  session: string;
}

const input = jsonCodec((value: unknown): Input => {
  if (typeof value !== "object" || value === null || !("command" in value)
    || !("model" in value) || typeof value.command !== "string" || typeof value.model !== "string") {
    throw new TypeError("expected command and model strings");
  }
  return { command: value.command, model: value.model };
});

const output = jsonCodec((value: unknown): Output => {
  if (typeof value !== "object" || value === null || !("session" in value)
    || typeof value.session !== "string") {
    throw new TypeError("expected a session string");
  }
  return { session: value.session };
});

export default defineProgram({
  input,
  output,
  async run(input) {
    const result = await session.create(input);
    if (result.kind !== "answer" || !("session" in result.value)) {
      throw new Error("session creation did not succeed");
    }
    return result.value;
  },
});
