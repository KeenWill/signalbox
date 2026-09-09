declare module "@signalbox/program-sdk/v1" {
  export interface Codec<T> {
    decode: (bytes: Uint8Array) => T;
    encode: (value: T) => Uint8Array;
  }

  type JsonData<T> = T extends string | number | boolean | null ? T
    : T extends Function ? never
    : T extends readonly unknown[] ? { [K in keyof T]: JsonData<T[K]> }
    : T extends object ? { [K in keyof T]: K extends string | number ? JsonData<T[K]> : never }
    : never;

  /** Validates JSON data; finite numbers, own properties and cycles are checked at runtime. */
  export function jsonCodec<T>(decode: (this: void, value: unknown) => T & JsonData<T>): Codec<T>;
  export function defineProgram<Input, Output>(definition: {
    input: Codec<Input>;
    output: Codec<Awaited<Output>>;
    run: (this: void, input: Input) => Output | Promise<Output>;
  }): (input: Uint8Array) => Promise<Uint8Array>;

  export type Capability = "time" | "random" | "sleep" | "subscribe" | "session"
    | "judge" | "exec-stage" | "corpus" | "eval-record" | "blob" | "register" | "repo-watch";
  export type Delivery =
    | { kind: "answer"; payload: number[] }
    | { kind: "wake"; payload: number[] }
    | { kind: "cancel"; payload: number[] }
    | { kind: "reject"; reason: "outstanding_requests" | "capability_denied" | "unsupported_operation" };
  export type EffectResult<T> =
    | { kind: "answer"; value: T }
    | Exclude<Delivery, { kind: "answer" }>;
  export function effect(capability: Capability, method: string, payload: Uint8Array): Promise<Delivery>;
  export function now(payload: Uint8Array): Promise<Delivery>;
  export function random(payload: Uint8Array): Promise<Delivery>;
  export function sleep(payload: Uint8Array): Promise<Delivery>;
  export function awaitEvent(payload: Uint8Array): Promise<Delivery>;

  export interface ProgramEventWait {
    source: { kind: "program_answers"; run: string };
    /** Journal position, zero to start at the beginning. */
    after: string;
  }
  export const primitives: {
    /** Unix milliseconds, represented as a u64 decimal string. */
    now(): Promise<EffectResult<string>>;
    /** Uniform full-width u64, represented as a decimal string. */
    random(): Promise<EffectResult<string>>;
    sleepUntil(deadlineUnixMs: string): Promise<
      { kind: "wake"; value: string } | Exclude<Delivery, { kind: "wake" }>
    >;
    awaitEvent(input: ProgramEventWait): Promise<EffectResult<{ position: string; payload: number[] }>>;
  };

  export interface RegisterInput {
    id: string;
    name: string;
    revision: string;
    source: number[];
    artifact: string;
    grants: Capability[];
  }
  export function register(input: RegisterInput): Promise<EffectResult<
    { registration: string } | { outcome: "ambiguous" }
  >>;

  export interface SessionCreateInput {
    command: string;
    model: string;
  }
  export interface SessionTurnInput {
    command: string;
    session: string;
    text: string;
    /** Positive u64 decimal digits, encoded without conversion through Number. */
    defaults_version: string;
  }
  export interface SessionTurnOutcome {
    session: string;
    turn: string;
    accepted_input: string;
    digest: number[];
    outcome: "completed" | "refused" | "failed" | "cancelled" | "retired" | "ambiguous";
  }
  export type SessionRefusal = { outcome: "refused" | "ambiguous" };
  export const session: {
    create: (input: SessionCreateInput) => Promise<EffectResult<{ session: string } | SessionRefusal>>;
    turn: (input: SessionTurnInput) => Promise<EffectResult<SessionTurnOutcome | SessionRefusal>>;
  };
}
