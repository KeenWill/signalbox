import { defineProgram, jsonCodec, session } from "@signalbox/program-sdk/v1";

const text = jsonCodec((value: unknown): string => {
  if (typeof value !== "string") throw new TypeError("expected string");
  return value;
});

defineProgram({ input: text, output: text, run: (input) => input.toUpperCase() });
// @ts-expect-error The output codec constrains the return type.
defineProgram({ input: text, output: text, run: () => 42 });
// @ts-expect-error An effect input must include the selected model.
session.create({ command: "missing model" });
// @ts-expect-error Full-width integers are decimal strings at the authoring boundary.
session.turn({ command: "command", session: "session", text: "text", defaults_version: 1 });

async function typedAnswer(): Promise<string | undefined> {
  const result = await session.create({ command: "command", model: "model" });
  // @ts-expect-error A delivery must be narrowed before reading a successful result.
  result.value.session;
  if (result.kind === "answer" && "session" in result.value) return result.value.session;
  return undefined;
}
void typedAnswer;
