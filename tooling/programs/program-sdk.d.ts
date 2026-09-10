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
    | "judge" | "exec-stage" | "corpus" | "eval-record" | "blob" | "register";
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
  export type ApprovalDisposition = "approve" | "deny" | "escalate_to_human";
  export interface JudgeBinding {
    selection: string; target: string; credential_reference: string;
    provider_model: string; contract_digest: string; cache_accounting: string;
  }
  export interface EvalManifest {
    corpus: string; format: "offline" | "live"; cases: number[]; repeats: number;
    binding: JudgeBinding; postures: Record<string, string>; speculative_tools: string[];
  }
  export interface JudgeUsage {
    input_tokens: string | null; output_tokens: string | null;
    cache_creation_input_tokens: string | null; cache_read_input_tokens: string | null;
  }
  export type JudgeAnswer =
    | { outcome: "ambiguous" }
    | { outcome: "failed"; call: string | null; request_digest: string; binding: JudgeBinding; cause: string; provider_reported_model: string | null; usage: JudgeUsage }
    | { outcome: "verdict"; call: string; request_digest: string; binding: JudgeBinding; actual: ApprovalDisposition; rationale: string; provider_reported_model: string | null; usage: JudgeUsage };
  export type CorpusCase =
    | { format: "offline"; case: { id: string; expected: ApprovalDisposition; label_provenance: string;
        request: { tool: string; arguments: string; commissioned_goal: string | null; session_template: string | null; frozen_system_prompt: string | null } } }
    | { format: "live"; case: { name: string; category: "git_push" | "thread_ops" | "network_egress" | "credential_access" | "destructive" | "workspace_benign" | "injection_resistance" | "context_absent" | "undecodable_arguments";
        tool: string; arguments: string; expected: ApprovalDisposition; goal: string | null; template: string | null; system_prompt: string | null; notes: string | null;
        dispatch: null | { repository: string; pull_request: string; head_sha: string; head_repository: string; head_branch: string; base_branch: string } } };
  export interface CorpusAnswer { cases: CorpusCase[]; corpus_digest: string; rendered_digest: string }
  export const evaluation: {
    manifest: Codec<EvalManifest>;
    corpus(): Promise<EffectResult<CorpusAnswer>>;
    judge(input: { trial: number }): Promise<EffectResult<JudgeAnswer>>;
    blob(input: { digest: string }): Promise<EffectResult<{ bytes: number[] }>>;
  };
}
