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

function modelCallDetailPage() {
  return {
    session_id: "00000000-0000-0000-0000-000000000991",
    items: [
      {
        address: { event_sequence: "8" },
        kind: "model_call_transition",
        body: {
          type: "model_call",
          turn_id: "00000000-0000-0000-0000-000000000992",
          model_call_id: "00000000-0000-0000-0000-000000000993",
          state: { type: "prepared" },
          model_identity_id: "00000000-0000-0000-0000-000000000994",
          request_context_items: "0",
          response: null,
          usage: {
            input_tokens: null,
            output_tokens: null,
            cache_creation_input_tokens: null,
            cache_read_input_tokens: null,
          },
          provider_failure_cause: null,
        },
        projected_body_bytes: 128,
      },
    ],
    projected_body_bytes: 128,
    continuation: null,
  };
}

function turnLifecycleDetailPage() {
  return {
    session_id: "00000000-0000-0000-0000-000000000991",
    items: [
      {
        address: { event_sequence: "9" },
        kind: "turn_completed",
        body: {
          type: "turn_lifecycle",
          turn_id: "00000000-0000-0000-0000-000000000992",
          lifecycle: "terminalized",
          cause_code: "completed",
        },
        projected_body_bytes: 128,
      },
    ],
    projected_body_bytes: 128,
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

test("generated detail decoder rejects an invalid turn identity", () => {
  const page = modelCallDetailPage();
  page.items[0].body.turn_id = "not-a-uuid";
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /one recognized variant/,
  );
});

test("generated detail decoder rejects an invalid model-call identity", () => {
  const page = modelCallDetailPage();
  page.items[0].body.model_call_id = "not-a-uuid";
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /one recognized variant/,
  );
});

test("generated detail decoder rejects an invalid model identity", () => {
  const page = modelCallDetailPage();
  page.items[0].body.model_identity_id = "not-a-uuid";
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /one recognized variant/,
  );
});

test("generated detail decoder rejects an invalid page session identity", () => {
  const page = userInputDetailPage();
  page.session_id = "not-a-uuid";
  assert.throws(() => decodeWebSessionTimelineDetailPage(page), /matching/);
});

test("generated detail decoder rejects invalid blob identities", () => {
  const page = userInputDetailPage();
  page.items[0].body.attachments = [
    { blob_id: "not-a-digest", length_bytes: "1", media_type: null },
  ];
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /one recognized variant/,
  );
});

test("generated detail decoder bounds attachments before decoding members", () => {
  const page = userInputDetailPage();
  page.items[0].body.attachments = Array.from({ length: 257 }, () => null);
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /one recognized variant/,
  );
});

test("generated detail decoder rejects responses on nonterminal calls", () => {
  const page = modelCallDetailPage();
  page.items[0].body.response = {
    text: "x",
    offset_bytes: "0",
    total_bytes: "1",
    continuation: null,
  };
  page.items[0].projected_body_bytes = 129;
  page.projected_body_bytes = 129;
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /terminal evidence only/,
  );
});

test("generated detail decoder rejects usage on nonterminal calls", () => {
  const page = modelCallDetailPage();
  page.items[0].body.usage.input_tokens = "1";
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /terminal evidence only/,
  );
});

test("generated detail decoder rejects failure causes on nonterminal calls", () => {
  const page = modelCallDetailPage();
  page.items[0].body.provider_failure_cause = "rate_limited";
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /terminal evidence only/,
  );
});

test("generated detail decoder rejects an unknown provider failure cause", () => {
  const page = modelCallDetailPage();
  page.items[0].body.state = {
    type: "terminal",
    disposition: "known_failed",
  };
  page.items[0].body.provider_failure_cause = "invented";
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /one recognized variant/,
  );
});

test("generated detail decoder accepts a cause-less known failure", () => {
  const page = modelCallDetailPage();
  page.items[0].body.state = {
    type: "terminal",
    disposition: "known_failed",
  };
  const decoded = decodeWebSessionTimelineDetailPage(page);
  assert.equal(decoded.items[0].body.state.disposition, "known_failed");
  assert.equal(decoded.items[0].body.provider_failure_cause, null);
});

test("generated detail decoder rejects a failure cause on another disposition", () => {
  const page = modelCallDetailPage();
  page.items[0].body.state = { type: "terminal", disposition: "completed" };
  page.items[0].body.provider_failure_cause = "rate_limited";
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /present only for a known_failed/,
  );
});

test("generated detail decoder bounds attachment media types", () => {
  const page = userInputDetailPage();
  page.items[0].body.attachments = [
    {
      blob_id: `sha256:${"a".repeat(64)}`,
      length_bytes: "1",
      media_type: `application/${"x".repeat(256)}`,
    },
  ];
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /one recognized variant/,
  );
});

test("generated detail decoder correlates lifecycle causes with event kinds", () => {
  const page = turnLifecycleDetailPage();
  page.items[0].body.cause_code = "failed";
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /the cause for turn_completed/,
  );
});

test("generated detail decoder rejects a nonzero input member index", () => {
  const page = userInputDetailPage();
  page.items[0].body.text.total_bytes = "6";
  page.items[0].body.text.continuation = {
    address: { event_sequence: "7" },
    field: "input_text",
    member_index: 1,
    offset_bytes: "3",
  };
  page.continuation = {
    type: "more_body",
    body: page.items[0].body.text.continuation,
  };
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /the projected member the excerpt belongs to/,
  );
});

test("generated detail decoder rejects a nonzero response member index", () => {
  const page = modelCallDetailPage();
  page.items[0].body.state = { type: "terminal", disposition: "completed" };
  page.items[0].body.response = {
    text: "abc",
    offset_bytes: "0",
    total_bytes: "6",
    continuation: {
      address: { event_sequence: "8" },
      field: "model_response",
      member_index: 1,
      offset_bytes: "3",
    },
  };
  page.items[0].projected_body_bytes = 131;
  page.projected_body_bytes = 131;
  page.continuation = {
    type: "more_body",
    body: page.items[0].body.response.continuation,
  };
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /the projected member the excerpt belongs to/,
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

test("generated detail decoder bounds excerpt text before byte accounting", () => {
  const page = userInputDetailPage();
  page.items[0].body.text.text = "x".repeat(65537);
  page.items[0].body.text.total_bytes = "65537";
  page.items[0].projected_body_bytes = 65665;
  page.projected_body_bytes = 65665;

  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /one recognized variant/,
  );
});

test("generated detail decoder requires a continued body to end the page", () => {
  const page = userInputDetailPage();
  page.items[0].body.text.total_bytes = "6";
  page.items[0].body.text.continuation = {
    address: { event_sequence: "7" },
    field: "input_text",
    member_index: 0,
    offset_bytes: "3",
  };
  page.items.push({
    address: { event_sequence: "8" },
    kind: "session_created",
    body: { type: "session_created", imported_evidence: null },
    projected_body_bytes: 128,
  });
  page.projected_body_bytes = 259;
  page.continuation = {
    type: "more_body",
    body: page.items[0].body.text.continuation,
  };

  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /absent after a continued body/,
  );
});

test("generated detail decoder rejects a continuation on an empty page", () => {
  const page = userInputDetailPage();
  page.items = [];
  page.projected_body_bytes = 0;
  page.continuation = {
    type: "more_at",
    address: { event_sequence: "9" },
  };

  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /absent on an empty page/,
  );
});

test("generated detail decoder rejects a response on a non-completed disposition", () => {
  const page = modelCallDetailPage();
  page.items[0].body.state = { type: "terminal", disposition: "refused" };
  page.items[0].body.response = {
    text: "x",
    offset_bytes: "0",
    total_bytes: "1",
    continuation: null,
  };
  page.items[0].projected_body_bytes = 129;
  page.projected_body_bytes = 129;

  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /present only for a completed terminal model call/,
  );
});

test("generated detail decoder rejects a non-ASCII attachment media type", () => {
  const page = userInputDetailPage();
  page.items[0].body.attachments = [
    {
      blob_id: `sha256:${"a".repeat(64)}`,
      length_bytes: "1",
      media_type: "application/café",
    },
  ];
  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /one recognized variant/,
  );
});

test("generated detail decoder rejects usage on a cancelled disposition", () => {
  const page = modelCallDetailPage();
  page.items[0].body.state = { type: "terminal", disposition: "cancelled" };
  page.items[0].body.usage.input_tokens = "1";

  assert.throws(
    () => decodeWebSessionTimelineDetailPage(page),
    /unreported for a cancelled terminal model call/,
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
