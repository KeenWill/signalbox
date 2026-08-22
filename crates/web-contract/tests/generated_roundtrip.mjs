import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  decodeWebApiErrorResponse,
  decodeWebContractBootstrap,
  decodeWebContractExample,
  decodeWebSessionTimelineDescriptor,
  decodeWebSessionTimelineDetailPage,
  decodeWebSessionTimelineWindow,
} from "../../../clients/web/src/generated/web-contract.mjs";

const fixtureUrl = new URL("./fixtures/example.json", import.meta.url);

function userInputDetailPage() {
  return {
    session_id: "00000000-0000-0000-0000-000000000991",
    items: [
      {
        address: { event_sequence: "7" },
        kind: "input_accepted",
        body: {
          type: "user_input",
          turn_id: "00000000-0000-0000-0000-000000000992",
          text: {
            text: "abc",
            offset_bytes: "0",
            total_bytes: "3",
            continuation: null,
          },
          attachments: [],
        },
        projected_body_bytes: 131,
      },
    ],
    projected_body_bytes: 131,
    continuation: null,
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
          bounded_session_timeline: true,
          bounded_session_timeline_detail: true,
          same_origin_json_mutations: true,
          ndjson_streaming: true,
        },
        limits: {
          max_json_body_bytes: 65536,
          max_ndjson_item_bytes: 65536,
          max_timeline_detail_items: 128,
          max_timeline_detail_bytes: 65536,
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

test("generated detail decoder rejects contradictory event semantics", () => {
  const page = userInputDetailPage();
  page.items[0].kind = "turn_failed";

  assert.throws(() => decodeWebSessionTimelineDetailPage(page), /input_accepted/);
});

test("generated detail decoder rejects invalid excerpt arithmetic", () => {
  const page = userInputDetailPage();
  page.items[0].body.text.offset_bytes = "10";
  page.items[0].body.text.total_bytes = "5";

  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /declared byte range/,
  );
});

test("generated detail decoder requires the exact excerpt continuation", () => {
  const page = userInputDetailPage();
  page.items[0].body.text.total_bytes = "6";
  page.items[0].body.text.continuation = {
    address: { event_sequence: "7" },
    field: "input_text",
    member_index: 0,
    offset_bytes: "4",
  };
  page.continuation = {
    type: "more_body",
    body: page.items[0].body.text.continuation,
  };

  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /immediately after the excerpt/,
  );
});

test("generated detail decoder rejects oversized arrays before their members", () => {
  const page = userInputDetailPage();
  page.items = Array.from({ length: 129 }, () => null);

  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /at most 128 items/,
  );
});

test("generated detail decoder rejects projected byte mismatches", () => {
  const page = userInputDetailPage();
  page.projected_body_bytes = 130;

  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /the computed 131 bytes/,
  );
});

test("generated detail decoder enforces the projected byte ceiling", () => {
  const page = userInputDetailPage();
  page.items[0].body.text.text = "x".repeat(65536);
  page.items[0].body.text.total_bytes = "65536";
  page.items[0].projected_body_bytes = 65664;
  page.projected_body_bytes = 65664;

  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /at most 65536 bytes/,
  );
});

test("generated detail decoder rejects non-monotonic addresses", () => {
  const page = userInputDetailPage();
  page.items.push({
    ...structuredClone(page.items[0]),
    address: { event_sequence: "7" },
  });
  page.projected_body_bytes = 262;

  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /strictly increasing/,
  );
});

test("generated detail decoder requires more-at to advance", () => {
  const page = userInputDetailPage();
  page.continuation = {
    type: "more_at",
    address: { event_sequence: "7" },
  };

  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /after the final returned item/,
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
