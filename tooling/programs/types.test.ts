import { defineProgram, jsonCodec, session } from "@signalbox/program-sdk/v1";
import type { Codec, SessionCreateInput, SessionTurnInput } from "@signalbox/program-sdk/v1";

const text = jsonCodec((value: unknown): string => {
  if (typeof value !== "string") throw new TypeError("expected string");
  return value;
});

// @ts-expect-error JSON validators cannot produce promises.
jsonCodec(async () => "ok");
// @ts-expect-error Explicit type arguments cannot admit promise values.
jsonCodec<Promise<string>>(async () => "ok");
// @ts-expect-error Undefined is outside the JSON value domain.
jsonCodec(() => undefined);
// @ts-expect-error Dates cannot be encoded as JSON data without conversion.
jsonCodec(() => new Date());
// @ts-expect-error Big integers require conversion to JSON strings.
jsonCodec(() => 1n);
// @ts-expect-error Functions are not JSON values.
jsonCodec(() => () => "ok");
// @ts-expect-error Non-JSON values nested in records are also rejected.
jsonCodec(() => ({ nested: { value: Promise.resolve("ok") } }));
// @ts-expect-error Array elements must be JSON values.
jsonCodec(() => ["ok", undefined]);

interface JsonRecord {
  label: string;
  nested: { count: number; active: boolean; empty: null };
  values: readonly (string | number)[];
  optional?: string;
}
const jsonRecord = jsonCodec((_value: unknown): JsonRecord => ({
  label: "checked", nested: { count: 1, active: true, empty: null }, values: ["ok", 1],
}));
const preservedRecordType: Codec<JsonRecord> = jsonRecord;
void preservedRecordType;

const receiverValidator = function (this: { prefix: string }, value: unknown): string {
  return this.prefix + String(value);
};
// @ts-expect-error JSON validators run without a receiver.
jsonCodec(receiverValidator);
jsonCodec(receiverValidator.bind({ prefix: "checked: " }));

const promisedText: Codec<Promise<string>> = {
  decode: async (bytes) => text.decode(bytes),
  encode: (_value) => new Uint8Array(),
};
// @ts-expect-error Output encoding receives the awaited string, not a promise.
defineProgram({ input: text, output: promisedText, run: async (input) => input });
// @ts-expect-error Explicit promise output types also require a codec for their awaited value.
defineProgram<string, Promise<string>>({ input: text, output: promisedText, run: async (input) => input });
defineProgram({ input: text, output: text, run: async (input) => input });
defineProgram<string, Promise<string>>({ input: text, output: text, run: async (input) => input });

// @ts-expect-error A widened create input would allow calls without a model.
const createWithoutModel: (input: Pick<SessionCreateInput, "command">) => ReturnType<typeof session.create> = session.create;
// @ts-expect-error A widened turn input would allow calls without a defaults version.
const turnWithoutVersion: (input: Omit<SessionTurnInput, "defaults_version">) => ReturnType<typeof session.turn> = session.turn;
void createWithoutModel;
void turnWithoutVersion;

const restrictedSession: typeof session = {
  // @ts-expect-error A create wrapper must accept every valid model string.
  create: (input: SessionCreateInput & { model: "only-model" }) => session.create(input),
  // @ts-expect-error A turn wrapper must accept every valid text string.
  turn: (input: SessionTurnInput & { text: "only-text" }) => session.turn(input),
};
void restrictedSession;

const onlyOk = jsonCodec((value: unknown): "ok" => {
  if (value !== "ok") throw new TypeError("expected ok");
  return value;
});

// @ts-expect-error A codec that only encodes "ok" cannot encode arbitrary strings.
const widened: Codec<string> = onlyOk;
// @ts-expect-error A codec that decodes arbitrary strings cannot promise only "ok".
const narrowed: Codec<"ok"> = text;
void widened;
void narrowed;

defineProgram({ input: text, output: onlyOk, run: (): "ok" => "ok" });
// @ts-expect-error The return type must match the narrow output codec.
defineProgram({ input: text, output: onlyOk, run: (): "other" => "other" });

defineProgram({ input: text, output: text, run: (input) => input.toUpperCase() });
defineProgram<string, string>({ input: text, output: text, run: (input: unknown) => String(input) });
// @ts-expect-error The callback must accept every value the input codec decodes.
defineProgram({ input: text, output: text, run: (input: "ok") => input });

defineProgram({
  input: text,
  output: text,
  run(input) {
    // @ts-expect-error The callback has no definition-object receiver.
    void this.input;
    return input;
  },
});
const receiverRun = function (this: { prefix: string }, input: string): string {
  return this.prefix + input;
};
// @ts-expect-error A callback requiring an explicit receiver cannot run unbound.
defineProgram({ input: text, output: text, run: receiverRun });

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
