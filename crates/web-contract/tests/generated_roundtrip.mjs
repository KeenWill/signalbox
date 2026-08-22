import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  decodeWebApiErrorResponse,
  decodeWebContractBootstrap,
  decodeWebContractExample,
  decodeWebSearchPage,
  decodeWebSessionTimelineDescriptor,
  decodeWebSessionTimelineWindow,
} from "../../../clients/web/src/generated/web-contract.mjs";

const fixtureUrl = new URL("./fixtures/example.json", import.meta.url);

function searchPage() {
  return {
    results: [
      {
        session_id: "00000000-0000-0000-0000-000000000991",
        address: { event_sequence: "1" },
        source: {
          kind: "session",
          session_id: "00000000-0000-0000-0000-000000000991",
        },
        content_class: "session_metadata",
        snippet: "café",
        highlights: [{ start_byte: 0, end_byte: 5 }],
      },
    ],
    continuation: {
      address: { event_sequence: "1" },
      projection_id: "1",
    },
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
          bounded_lexical_search: true,
          bounded_session_timeline: true,
          same_origin_json_mutations: true,
          ndjson_streaming: true,
        },
        limits: {
          max_json_body_bytes: 65536,
          max_ndjson_item_bytes: 65536,
          max_search_page_items: 100,
          max_search_query_bytes: 512,
          max_search_snippet_bytes: 512,
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

test("generated search decoder rejects an invalid projection identity", () => {
  const page = searchPage();
  page.continuation.projection_id = "0";

  assert.throws(
    () => decodeWebSearchPage(page),
    /continuation must be one recognized variant/,
  );
});

test("generated search decoder rejects more than one bounded page", () => {
  const page = searchPage();
  page.results = Array.from({ length: 101 }, () => page.results[0]);

  assert.throws(
    () => decodeWebSearchPage(page),
    /results must be at most 100 items/,
  );
});

test("generated search decoder rejects an oversized UTF-8 snippet", () => {
  const page = searchPage();
  page.results[0].snippet = "é".repeat(257);
  page.results[0].highlights = [];

  assert.throws(
    () => decodeWebSearchPage(page),
    /snippet must be at most 512 UTF-8 bytes/,
  );
});

test("generated search decoder rejects a highlight inside a UTF-8 character", () => {
  const page = searchPage();
  page.results[0].highlights = [{ start_byte: 4, end_byte: 5 }];

  assert.throws(
    () => decodeWebSearchPage(page),
    /range on UTF-8 boundaries/,
  );
});

test("generated search decoder rejects overlapping highlight ranges", () => {
  const page = searchPage();
  page.results[0].highlights = [
    { start_byte: 0, end_byte: 3 },
    { start_byte: 2, end_byte: 5 },
  ];

  assert.throws(
    () => decodeWebSearchPage(page),
    /ordered non-overlapping in-bounds UTF-8 byte range/,
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
