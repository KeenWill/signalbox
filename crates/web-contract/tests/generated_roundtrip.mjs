import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  decodeWebApiErrorResponse,
  decodeWebContractBootstrap,
  decodeWebContractExample,
  decodeWebSessionTimelineDescriptor,
  decodeWebSessionTimelineWindow,
  decodeWebUsageCallPage,
  decodeWebUsageSummary,
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
          bounded_lexical_search: true,
          bounded_session_timeline: true,
          bounded_usage_cost: true,
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
          max_usage_aggregate_groups: 256,
          max_usage_call_page_items: 100,
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

test("generated usage decoder preserves nullable axes and labeled cost", () => {
  const summary = decodeWebUsageSummary({
    groups: [
      {
        call_kind: "model_call",
        model_id: "00000000-0000-0000-0000-000000000041",
        provenance: "estimated",
        input_semantics: "cache_exclusive",
        coverage: {
          input: true,
          output: false,
          cache_creation_input: false,
          cache_read_input: false,
        },
        call_count: "2",
        tokens: {
          input: "17",
          output: null,
          cache_creation_input: null,
          cache_read_input: null,
        },
        cost: {
          status: "derived",
          amount_usd: "0.17",
          rate_version: "fixture-v2",
          label: "metered_equivalent",
        },
      },
    ],
    truncated: false,
  });

  assert.equal(summary.groups[0].tokens.output, null);
  assert.equal(summary.groups[0].cost.status, "derived");
  assert.equal(summary.groups[0].cost.rate_version, "fixture-v2");
  assert.equal(summary.groups[0].cost.label, "metered_equivalent");
});

function usageGroup() {
  return {
    call_kind: "model_call",
    model_id: "00000000-0000-0000-0000-000000000041",
    provenance: "estimated",
    input_semantics: "cache_exclusive",
    coverage: {
      input: true,
      output: false,
      cache_creation_input: false,
      cache_read_input: false,
    },
    call_count: "1",
    tokens: {
      input: "17",
      output: null,
      cache_creation_input: null,
      cache_read_input: null,
    },
    cost: {
      status: "derived",
      amount_usd: "0.17",
      rate_version: "fixture-v2",
      label: "metered_equivalent",
    },
  };
}

function usageCall() {
  return {
    call_kind: "model_call",
    call_id: "00000000-0000-0000-0000-000000000051",
    session_id: "00000000-0000-0000-0000-000000000052",
    turn_id: "00000000-0000-0000-0000-000000000053",
    model_id: "00000000-0000-0000-0000-000000000041",
    provenance: "estimated",
    input_semantics: "cache_exclusive",
    tokens: {
      input: "17",
      output: null,
      cache_creation_input: null,
      cache_read_input: null,
    },
    recorded_at_micros: "1777777777123456",
    cost: {
      status: "derived",
      amount_usd: "0.17",
      rate_version: "fixture-v2",
      label: "metered_equivalent",
    },
  };
}

test("generated usage decoder enforces collection ceilings", () => {
  assert.throws(
    () => decodeWebUsageSummary({
      groups: Array.from({ length: 257 }, usageGroup),
      truncated: true,
    }),
    /at most 256 items/,
  );
  assert.throws(
    () => decodeWebUsageCallPage({
      calls: Array.from({ length: 101 }, usageCall),
      continuation: null,
    }),
    /at most 100 items/,
  );
});

test("generated usage decoder rejects noncanonical dollar amounts", () => {
  const trailingZero = usageGroup();
  trailingZero.cost.amount_usd = "0.170";
  const oversizedCoefficient = usageGroup();
  oversizedCoefficient.cost.amount_usd = "79228162514264337593543950336";

  assert.throws(
    () => decodeWebUsageSummary({ groups: [trailingZero], truncated: false }),
    /one recognized variant/,
  );
  assert.throws(
    () => decodeWebUsageSummary({ groups: [oversizedCoefficient], truncated: false }),
    /one recognized variant/,
  );
});

test("generated usage summary rejects contradictory coverage", () => {
  const group = usageGroup();
  group.coverage.input = false;

  assert.throws(
    () => decodeWebUsageSummary({ groups: [group], truncated: false }),
    /consistent with token evidence/,
  );
});

test("generated summary and call decoders reject derived cost without tokens", () => {
  const group = usageGroup();
  group.coverage.input = false;
  group.tokens.input = null;
  const call = usageCall();
  call.tokens.input = null;

  assert.throws(
    () => decodeWebUsageSummary({ groups: [group], truncated: false }),
    /unavailable without token evidence/,
  );
  assert.throws(
    () => decodeWebUsageCallPage({ calls: [call], continuation: null }),
    /unavailable without token evidence/,
  );
});
