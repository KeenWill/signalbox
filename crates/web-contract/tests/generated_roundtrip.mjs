import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  decodeWebApiErrorResponse,
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
