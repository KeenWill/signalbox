import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  decodeWebApiErrorResponse,
  decodeWebAttentionSnapshot,
  decodeWebAttentionStreamEvent,
  decodeWebContractBootstrap,
  decodeWebContractExample,
} from "../../../clients/web/src/generated/web-contract.mjs";

const fixtureUrl = new URL("./fixtures/example.json", import.meta.url);

test("generated example decoder round trips the Rust fixture", async () => {
  const source = JSON.parse(await readFile(fixtureUrl, "utf8"));
  const decoded = decodeWebContractExample(source);
  const roundTripped = JSON.parse(JSON.stringify(decoded));

  assert.deepEqual(roundTripped, source);
});

test("generated example decoder rejects unknown fields", () => {
  assert.throws(
    () =>
      decodeWebContractExample({
        request_id: "contract-round-trip",
        message: "browser contract fixture",
        process_protocol_frame: {},
      }),
    /example\.process_protocol_frame must be absent/,
  );
});

test("generated bootstrap decoder rejects another contract version", () => {
  assert.throws(
    () =>
      decodeWebContractBootstrap({
        contract: { name: "signalbox.web-http", version: "2" },
        capabilities: {
          bounded_json: true,
          same_origin_json_mutations: true,
          ndjson_streaming: true,
        },
        limits: {
          max_json_body_bytes: 65536,
          max_ndjson_item_bytes: 65536,
        },
      }),
    /incompatible web contract/,
  );
});

test("generated error decoder preserves the transport application boundary", () => {
  const transport = decodeWebApiErrorResponse({
    error: {
      kind: "transport",
      code: "invalid_json",
      message: "request body is not the expected JSON value",
    },
  });

  assert.equal(transport.error.kind, "transport");
  assert.throws(
    () =>
      decodeWebApiErrorResponse({
        error: {
          kind: "process_protocol",
          code: "frame_rejected",
          message: "not a browser error layer",
        },
      }),
    /one recognized variant/,
  );
});

const idleSummary = {
  session_id: "00000000-0000-0000-0000-000000000001",
  current_turn_id: null,
  state: "idle",
  action: null,
  goal_block: null,
  judge: { actionable: "0", completed: "0", escalated: "0", failed: "0" },
  last_activity: { unix_milliseconds: "0", kind: "session" },
};

test("generated snapshot decoder rejects an action inconsistent with state", () => {
  assert.throws(
    () =>
      decodeWebAttentionSnapshot({
        cursor: "1",
        summaries: [{ ...idleSummary, action: "decide_approval" }],
        continuation_after_session_id: null,
      }),
    /attention_snapshot\.summaries\[0\]\.action must be consistent with attention state "idle"/,
  );
});

test("generated stream decoder rejects a missing blocked action", () => {
  assert.throws(
    () =>
      decodeWebAttentionStreamEvent({
        kind: "update",
        cursor: "2",
        summaries: [{ ...idleSummary, state: "blocked" }],
      }),
    /attention_event\.summaries\[0\]\.action must be consistent with attention state "blocked"/,
  );
});
