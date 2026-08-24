import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  decodeWebApiErrorResponse,
  decodeWebAttentionSnapshot,
  decodeWebAttentionStreamEvent,
  decodeWebContractBootstrap,
  decodeWebContractExample,
  decodeWebSessionLiveSnapshot,
  decodeWebSessionLiveStreamEvent,
  decodeWebSessionTimelineDescriptor,
  decodeWebSessionTimelineWindow,
} from "../../../clients/web/src/generated/web-contract.mjs";

const fixtureUrl = new URL("./fixtures/example.json", import.meta.url);

function thirtyThreeQueuedTurnIds() {
  return Array.from({ length: 33 }, (_, index) => `${index}`);
}

function attentionSummary(overrides = {}) {
  return {
    session_id: "00000000-0000-0000-0000-000000000991",
    title_summary: null,
    title_truncated: false,
    archived: false,
    current_turn_id: null,
    active_turn_count: "0",
    queued_turn_count: "0",
    state: "idle",
    action: null,
    goal_block: null,
    judge: { actionable: "0", completed: "0", escalated: "0", failed: "0" },
    last_activity: { unix_microseconds: "1", kind: "session" },
    ...overrides,
  };
}

function attentionSnapshot(overrides = {}) {
  return {
    cursor: "1",
    total: "1",
    sort: "last_activity_descending",
    summaries: [attentionSummary()],
    continuation: null,
    ...overrides,
  };
}

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
          bounded_session_live: true,
          bounded_session_timeline: true,
          same_origin_json_mutations: true,
          ndjson_streaming: true,
        },
        limits: {
          max_json_body_bytes: 65536,
          max_ndjson_item_bytes: 65536,
          max_session_live_queued_turns: 32,
          max_timeline_window_items: 256,
          max_timeline_window_bytes: 65536,
        },
      }),
    /incompatible web contract/,
  );
});

test("generated live decoder bounds retained queued turns", () => {
  assert.throws(
    () =>
      decodeWebSessionLiveSnapshot({
        session_id: "00000000-0000-0000-0000-000000000991",
        observed_through: "7",
        active: null,
        queued_turn_count: "33",
        queued_turn_ids: thirtyThreeQueuedTurnIds(),
        reconciliation: null,
        runner: null,
      }),
    /at most 32 items/,
  );
});

test("generated live decoder correlates queued preview with its count", () => {
  assert.throws(
    () =>
      decodeWebSessionLiveSnapshot({
        session_id: "00000000-0000-0000-0000-000000000991",
        observed_through: "7",
        active: null,
        queued_turn_count: "0",
        queued_turn_ids: ["00000000-0000-0000-0000-000000000992"],
        reconciliation: null,
        runner: null,
      }),
    /exactly 0 IDs for queued_turn_count/,
  );
});

test("generated live decoder rejects duplicate queued turn identities", () => {
  const duplicate = "00000000-0000-0000-0000-000000000992";

  assert.throws(
    () =>
      decodeWebSessionLiveSnapshot({
        session_id: "00000000-0000-0000-0000-000000000991",
        observed_through: "7",
        active: null,
        queued_turn_count: "2",
        queued_turn_ids: [duplicate, duplicate],
        reconciliation: null,
        runner: null,
      }),
    /unique turn IDs/,
  );
});

test("generated live decoder validates identities and active-state correlation", () => {
  assert.throws(
    () =>
      decodeWebSessionLiveSnapshot({
        session_id: "not-a-uuid",
        observed_through: "7",
        active: null,
        queued_turn_count: "0",
        queued_turn_ids: [],
        reconciliation: null,
        runner: null,
      }),
    /matching/,
  );
  assert.throws(
    () =>
      decodeWebSessionLiveSnapshot({
        session_id: "00000000-0000-0000-0000-000000000991",
        observed_through: "7",
        active: {
          turn_id: "00000000-0000-0000-0000-000000000992",
          state: { kind: "running", model_call_id: null },
        },
        queued_turn_count: "0",
        queued_turn_ids: [],
        reconciliation: {
          kind: "model_call",
          turn_id: "00000000-0000-0000-0000-000000000992",
          model_call_id: "00000000-0000-0000-0000-000000000993",
        },
        runner: null,
      }),
    /absent while an active turn is present/,
  );
});

test("generated live decoder treats omitted optional state as absent", () => {
  const snapshot = decodeWebSessionLiveSnapshot({
    session_id: "00000000-0000-0000-0000-000000000991",
    observed_through: "7",
    queued_turn_count: "0",
    queued_turn_ids: [],
  });

  assert.equal(snapshot.active, undefined);
  assert.equal(snapshot.reconciliation, undefined);
});

test("generated live decoder rejects malformed runner correlations", () => {
  assert.throws(
    () =>
      decodeWebSessionLiveSnapshot({
        session_id: "00000000-0000-0000-0000-000000000991",
        observed_through: "7",
        active: null,
        queued_turn_count: "0",
        queued_turn_ids: [],
        reconciliation: null,
        runner: { state: "pinned", placement_revision: "1" },
      }),
    /one recognized variant/,
  );
});

test("generated live decoder requires positive placement revisions", () => {
  assert.throws(
    () =>
      decodeWebSessionLiveSnapshot({
        session_id: "00000000-0000-0000-0000-000000000991",
        observed_through: "7",
        active: null,
        queued_turn_count: "0",
        queued_turn_ids: [],
        reconciliation: null,
        runner: { state: "unpinned", placement_revision: "0" },
      }),
    /one recognized variant/,
  );
});

test("generated live stream decoder rejects variant-only extra fields", () => {
  assert.throws(
    () =>
      decodeWebSessionLiveStreamEvent({
        kind: "provider_text_delta",
        turn_id: "00000000-0000-0000-0000-000000000992",
        model_call_id: "00000000-0000-0000-0000-000000000993",
        part_index: 0,
        content: "draft",
        cursor: "8",
      }),
    /one recognized variant/,
  );
});

test("generated live stream decoder correlates durable cursor and address", () => {
  assert.throws(
    () =>
      decodeWebSessionLiveStreamEvent({
        kind: "durable",
        cursor: "8",
        address: { event_sequence: "9" },
        event_kind: "turn_activated",
      }),
    /equal to cursor/,
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

test("generated attention decoder validates decimals and identities", () => {
  assert.throws(
    () => decodeWebAttentionSnapshot(attentionSnapshot({ cursor: "garbage" })),
    /matching/,
  );
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({
          summaries: [attentionSummary({ session_id: "not-a-uuid" })],
        }),
      ),
    /matching/,
  );
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({
          summaries: [attentionSummary({ current_turn_id: "not-a-uuid" })],
        }),
      ),
    /one recognized variant/,
  );
});

test("generated attention decoder rejects unknown envelope fields", () => {
  assert.throws(
    () =>
      decodeWebAttentionStreamEvent({
        kind: "resync_required",
        cursor: "1",
        incompatible: true,
      }),
    /one recognized variant/,
  );
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({
          continuation: {
            kind: "session_identity",
            session_id: "00000000-0000-0000-0000-000000000991",
            incompatible: true,
          },
        }),
      ),
    /one recognized variant/,
  );
});

test("generated attention decoder requires the continuation field", () => {
  const { continuation: _continuation, ...withoutContinuation } = attentionSnapshot();

  assert.throws(
    () => decodeWebAttentionSnapshot(withoutContinuation),
    /continuation must be present/,
  );
  assert.equal(decodeWebAttentionSnapshot(attentionSnapshot()).continuation, null);
});

test("generated attention decoder enforces collection and scalar bounds", () => {
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({
          summaries: Array.from({ length: 33 }, () => attentionSummary()),
        }),
      ),
    /at most 16 items/,
  );
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({
          summaries: [attentionSummary({ title_summary: "x".repeat(129) })],
        }),
      ),
    /at most 128 Unicode scalar values/,
  );
});

test("generated attention decoder rejects inconsistent operator actions", () => {
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({
          summaries: [attentionSummary({ state: "awaiting_approval", action: null })],
        }),
      ),
    /action required by state awaiting_approval/,
  );
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({
          summaries: [
            attentionSummary({
              state: "blocked",
              action: "provide_goal_need",
              goal_block: null,
            }),
          ],
        }),
      ),
    /present exactly for blocked state/,
  );
});
