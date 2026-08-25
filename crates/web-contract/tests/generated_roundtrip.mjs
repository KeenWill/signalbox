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

test("generated live decoder requires a positive observation cursor", () => {
  assert.throws(
    () =>
      decodeWebSessionLiveSnapshot({
        session_id: "00000000-0000-0000-0000-000000000991",
        observed_through: "0",
        active: null,
        queued_turn_count: "0",
        queued_turn_ids: [],
        reconciliation: null,
        runner: null,
      }),
    /matching/,
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

test("generated live decoder rejects queued identities occupying current state", () => {
  const occupiedTurn = "00000000-0000-0000-0000-000000000992";

  assert.throws(
    () =>
      decodeWebSessionLiveSnapshot({
        session_id: "00000000-0000-0000-0000-000000000991",
        observed_through: "7",
        active: {
          turn_id: occupiedTurn,
          state: { kind: "running", model_call_id: null },
        },
        queued_turn_count: "1",
        queued_turn_ids: [occupiedTurn],
        reconciliation: null,
        runner: null,
      }),
    /disjoint from active and reconciliation turn IDs/,
  );
});

test("generated live decoder validates identities", () => {
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
});

test("generated live decoder rejects simultaneous active and reconciliation states", () => {
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

test("generated live decoder rejects self-referential child waits", () => {
  const sessionId = "00000000-0000-0000-0000-000000000991";

  assert.throws(
    () =>
      decodeWebSessionLiveSnapshot({
        session_id: sessionId,
        observed_through: "7",
        active: {
          turn_id: "00000000-0000-0000-0000-000000000992",
          state: {
            kind: "awaiting_child",
            tool_request_id: "00000000-0000-0000-0000-000000000993",
            child_session_id: sessionId,
          },
        },
        queued_turn_count: "0",
        queued_turn_ids: [],
        reconciliation: null,
        runner: null,
      }),
    /different from the parent session ID/,
  );
});

test("generated live decoder correlates runner recovery with placement", () => {
  assert.throws(
    () =>
      decodeWebSessionLiveSnapshot({
        session_id: "00000000-0000-0000-0000-000000000991",
        observed_through: "7",
        active: {
          turn_id: "00000000-0000-0000-0000-000000000992",
          state: {
            kind: "awaiting_runner_recovery",
            runner_id: "00000000-0000-0000-0000-000000000993",
            placement_revision: "4",
          },
        },
        queued_turn_count: "0",
        queued_turn_ids: [],
        reconciliation: null,
        runner: {
          state: "runner_lost",
          runner_id: "00000000-0000-0000-0000-000000000994",
          placement_revision: "4",
        },
      }),
    /runner placement required by awaiting_runner_recovery/,
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

test("generated live stream decoder bounds provider text fragments", () => {
  const admitted = decodeWebSessionLiveStreamEvent({
    kind: "provider_text_delta",
    turn_id: "00000000-0000-0000-0000-000000000992",
    model_call_id: "00000000-0000-0000-0000-000000000993",
    part_index: 0,
    content: "x".repeat(8192),
  });
  assert.equal(admitted.content.length, 8192);
  assert.throws(
    () =>
      decodeWebSessionLiveStreamEvent({
        kind: "provider_text_delta",
        turn_id: "00000000-0000-0000-0000-000000000992",
        model_call_id: "00000000-0000-0000-0000-000000000993",
        part_index: 0,
        content: "x".repeat(8193),
      }),
    /at most 8192 UTF-8 bytes/,
  );
  assert.throws(
    () =>
      decodeWebSessionLiveStreamEvent({
        kind: "provider_text_delta",
        turn_id: "00000000-0000-0000-0000-000000000992",
        model_call_id: "00000000-0000-0000-0000-000000000993",
        part_index: 0,
        content: "\u{20AC}".repeat(2731),
      }),
    /at most 8192 UTF-8 bytes/,
  );
});

test("generated live stream decoder requires a positive resynchronization cursor", () => {
  assert.throws(
    () =>
      decodeWebSessionLiveStreamEvent({
        kind: "resync_required",
        cursor: "0",
      }),
    /one recognized variant/,
  );
  const resync = decodeWebSessionLiveStreamEvent({
    kind: "resync_required",
    cursor: "7",
  });
  assert.equal(resync.cursor, "7");
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

test("generated attention decoder requires the nullable current turn field", () => {
  const withoutCurrentTurn = attentionSummary();
  delete withoutCurrentTurn.current_turn_id;

  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({ summaries: [withoutCurrentTurn] }),
      ),
    /current_turn_id must be present/,
  );
});

test("generated attention decoder requires identities for turn-backed states", () => {
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({
          summaries: [attentionSummary({ state: "active" })],
        }),
      ),
    /current_turn_id must be a turn identity for state active/,
  );
});

test("generated attention decoder rejects totals below the returned page", () => {
  assert.throws(
    () => decodeWebAttentionSnapshot(attentionSnapshot({ total: "0" })),
    /total must be at least the number of returned summaries/,
  );
});

test("generated attention decoder rejects contradictory title truncation", () => {
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({
          summaries: [attentionSummary({ title_truncated: true })],
        }),
      ),
    /title_truncated must be false when title_summary is null/,
  );
});

test("generated attention decoder rejects unpaired text surrogates", () => {
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({
          summaries: [attentionSummary({ title_summary: "\ud800" })],
        }),
      ),
    /well-formed Unicode text/,
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
      decodeWebAttentionStreamEvent({
        kind: "update",
        cursor: "1",
        summaries: [],
      }),
    /one recognized variant/,
  );
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
      decodeWebAttentionStreamEvent({
        kind: "update",
        cursor: "1",
        summaries: Array.from({ length: 17 }, () => attentionSummary()),
      }),
    /one recognized variant/,
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
  const blockedWithoutGoalBlock = attentionSummary({
    state: "blocked",
    action: "provide_goal_need",
  });
  delete blockedWithoutGoalBlock.goal_block;
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({ summaries: [blockedWithoutGoalBlock] }),
      ),
    /present exactly for blocked state/,
  );
});

test("generated attention decoder rejects continuation and sort mismatches", () => {
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({
          sort: "session_identity_ascending",
          continuation: {
            kind: "last_activity",
            unix_microseconds: "1",
            session_id: "00000000-0000-0000-0000-000000000991",
          },
        }),
      ),
    /continuation required by sort session_identity_ascending/,
  );
  assert.throws(
    () =>
      decodeWebAttentionSnapshot(
        attentionSnapshot({
          continuation: {
            kind: "session_identity",
            session_id: "00000000-0000-0000-0000-000000000991",
          },
        }),
      ),
    /continuation required by sort last_activity_descending/,
  );
});
