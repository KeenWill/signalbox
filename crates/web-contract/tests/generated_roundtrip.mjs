import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  decodeWebApiErrorResponse,
  decodeWebAttentionSnapshot,
  decodeWebAttentionStreamEvent,
  decodeWebContractBootstrap,
  decodeWebContractExample,
  decodeWebSessionTimelineDescriptor,
  decodeWebSessionTimelineWindow,
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
          bounded_session_timeline: true,
          same_origin_json_mutations: true,
          ndjson_streaming: true,
        },
        limits: {
          max_json_body_bytes: 65536,
          max_ndjson_item_bytes: 65536,
          max_timeline_window_items: 256,
          max_timeline_window_bytes: 65536,
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
        summaries: [
          {
            ...idleSummary,
            state: "blocked",
            goal_block: {
              generation: "1",
              reason: "user_input_required",
              need_summary: "need",
            },
          },
        ],
      }),
    /attention_event\.summaries\[0\]\.action must be consistent with attention state "blocked"/,
  );
});

test("generated snapshot decoder treats an omitted optional action as null", () => {
  const { action: _action, ...withoutAction } = idleSummary;
  assert.doesNotThrow(() => decodeWebAttentionSnapshot({
    cursor: "1", summaries: [withoutAction], continuation_after_session_id: null,
  }));
});

test("generated snapshot decoder rejects goal evidence for an idle state", () => {
  assert.throws(() => decodeWebAttentionSnapshot({
    cursor: "1",
    summaries: [{ ...idleSummary, goal_block: { generation: "1", reason: "user_input_required", need_summary: "need" } }],
    continuation_after_session_id: null,
  }), /goal_block must be consistent with attention state "idle"/);
});

test("generated snapshot decoder admits optional runner-loss evidence", () => {
  assert.doesNotThrow(() => decodeWebAttentionSnapshot({
    cursor: "1", summaries: [{ ...idleSummary, state: "runner_lost" }], continuation_after_session_id: null,
  }));
});

test("generated snapshot decoder admits evidence retained by runner loss", () => {
  assert.doesNotThrow(() => decodeWebAttentionSnapshot({
    cursor: "1",
    summaries: [{ ...idleSummary, state: "runner_lost", goal_block: { generation: "1", reason: "execution_failure", need_summary: "need" } }],
    continuation_after_session_id: null,
  }));
});

test("generated snapshot decoder rejects unpaired UTF-16 surrogates", () => {
  assert.throws(() => decodeWebAttentionSnapshot({
    cursor: "1",
    summaries: [{ ...idleSummary, state: "blocked", action: "provide_goal_need", goal_block: { generation: "1", reason: "user_input_required", need_summary: "\ud800" } }],
    continuation_after_session_id: null,
  }), /one recognized variant/);
});

test("generated snapshot decoder rejects the removed restore action", () => {
  assert.throws(() => decodeWebAttentionSnapshot({
    cursor: "1", summaries: [{ ...idleSummary, action: "restore_runner" }], continuation_after_session_id: null,
  }), /one recognized variant/);
});

test("generated snapshot decoder rejects a mismatched continuation", () => {
  assert.throws(() => decodeWebAttentionSnapshot({
    cursor: "1",
    summaries: [idleSummary],
    continuation_after_session_id: "00000000-0000-0000-0000-000000000002",
  }), /continuation_after_session_id must be the last returned session identity/);
});

test("generated stream decoder rejects a continuation for an empty page", () => {
  assert.throws(() => decodeWebAttentionStreamEvent({
    kind: "snapshot",
    snapshot: {
      cursor: "1",
      summaries: [],
      continuation_after_session_id: "00000000-0000-0000-0000-000000000001",
    },
  }), /continuation_after_session_id must be the last returned session identity/);
});

test("generated timeline decoder rejects an address beyond u64", () => {
  assert.throws(
    () =>
      decodeWebSessionTimelineWindow({
        session_id: "00000000-0000-0000-0000-000000000991",
        items: [
          {
            address: { event_sequence: "18446744073709551616" },
            kind: "input_accepted",
            projected_structured_bytes: 96,
          },
        ],
        projected_structured_bytes: 96,
        continuation_before: null,
        continuation_after: null,
      }),
    /unsigned 64-bit integer/,
  );
});

test("generated timeline decoder rejects an overlong decimal before BigInt", () => {
  assert.throws(
    () =>
      decodeWebSessionTimelineWindow({
        session_id: "00000000-0000-0000-0000-000000000991",
        items: [
          {
            address: { event_sequence: "1".repeat(1000) },
            kind: "session_created",
            projected_structured_bytes: 79,
          },
        ],
        projected_structured_bytes: 79,
        continuation_before: null,
        continuation_after: null,
      }),
    /unsigned 64-bit integer/,
  );
});

test("generated descriptor decoder rejects a fact beyond u64", () => {
  assert.throws(
    () =>
      decodeWebSessionTimelineDescriptor({
        session_id: "00000000-0000-0000-0000-000000000991",
        sizes: {
          item_count: "18446744073709551616",
          projected_text_bytes: "0",
          projected_structured_bytes: "96",
          referenced_blob_count: "0",
          referenced_blob_bytes: "0",
        },
        first_address: { event_sequence: "1" },
        latest_address: { event_sequence: "1" },
        work: { active_turn_count: "0", queued_turn_count: "0" },
        observed_through: "1",
      }),
    /unsigned 64-bit integer/,
  );
});

test("generated descriptor decoder rejects an invalid session ID", () => {
  assert.throws(
    () =>
      decodeWebSessionTimelineDescriptor({
        session_id: "not-a-uuid",
        sizes: {
          item_count: "1",
          projected_text_bytes: "0",
          projected_structured_bytes: "96",
          referenced_blob_count: "0",
          referenced_blob_bytes: "0",
        },
        first_address: { event_sequence: "1" },
        latest_address: { event_sequence: "1" },
        work: { active_turn_count: "0", queued_turn_count: "0" },
        observed_through: "1",
      }),
    /matching/,
  );
});

test("generated window decoder rejects an invalid session ID", () => {
  assert.throws(
    () =>
      decodeWebSessionTimelineWindow({
        session_id: "not-a-uuid",
        items: [],
        projected_structured_bytes: 0,
        continuation_before: null,
        continuation_after: null,
      }),
    /matching/,
  );
});

test("generated window decoder requires both continuation fields", () => {
  assert.throws(
    () =>
      decodeWebSessionTimelineWindow({
        session_id: "00000000-0000-0000-0000-000000000991",
        items: [],
        projected_structured_bytes: 0,
        continuation_before: null,
      }),
    /continuation_after must be present/,
  );
});
