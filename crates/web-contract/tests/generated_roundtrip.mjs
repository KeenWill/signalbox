import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  decodeWebApiErrorResponse,
  decodeWebContractBootstrap,
  decodeWebContractExample,
  decodeWebImportListPage,
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
        contract: { name: "signalbox.web-http", version: "999" },
        capabilities: {
          bounded_json: true,
          same_origin_json_mutations: true,
          ndjson_streaming: true,
          import_discovery: true,
          imported_continuations: true,
        },
        limits: {
          max_json_body_bytes: 65536,
          max_ndjson_item_bytes: 65536,
        },
      }),
    /incompatible web contract/,
  );
});

test("generated bootstrap decoder rejects a disabled required capability", () => {
  assert.throws(
    () =>
      decodeWebContractBootstrap({
        contract: { name: "signalbox.web-http", version: "2" },
        capabilities: {
          bounded_json: true,
          same_origin_json_mutations: true,
          ndjson_streaming: true,
          import_discovery: false,
          imported_continuations: true,
        },
        limits: {
          max_json_body_bytes: 65536,
          max_ndjson_item_bytes: 65536,
        },
      }),
    /incompatible web contract/,
  );
});

test("generated bootstrap decoder rejects incompatible limits", () => {
  assert.throws(
    () =>
      decodeWebContractBootstrap({
        contract: { name: "signalbox.web-http", version: "2" },
        capabilities: {
          bounded_json: true,
          same_origin_json_mutations: true,
          ndjson_streaming: true,
          import_discovery: true,
          imported_continuations: true,
        },
        limits: {
          max_json_body_bytes: 1,
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

test("generated imports decoder accepts explicit null evidence and cursor", () => {
  const page = {
    items: [
      {
        imported_conversation_id: "00000000-0000-7000-8000-000000000001",
        display_title: null,
        format: "codex_rollout_jsonl_v1",
        source_session_id: null,
        entry_count: 1,
      },
    ],
    next_cursor: null,
  };

  assert.deepEqual(decodeWebImportListPage(page), page);
});
