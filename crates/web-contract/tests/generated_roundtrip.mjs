import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  decodeWebApiErrorResponse,
  decodeWebBlobDescriptor,
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
        contract: { name: "signalbox.web-http", version: "1" },
        capabilities: {
          bounded_json: true,
          same_origin_json_mutations: true,
          ndjson_streaming: true,
          immutable_blob_content: true,
          blob_derivations: true,
          image_derivatives: true,
        },
        limits: {
          max_json_body_bytes: 65536,
          max_ndjson_item_bytes: 65536,
        },
      }),
    /incompatible web contract/,
  );
});

test("generated blob decoder accepts capability-projected views", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const descriptor = decodeWebBlobDescriptor({
    digest,
    byte_length: "94371840",
    declared_media_type: "image/png",
    display_filename: ["capture.png"],
    available_views: [
      {
        kind: "thumbnail",
        content_url: `/api/blobs/${digest}/content/image-png`,
        media_type: "image/png",
        byte_length: "2048",
        derivations: [
          {
            derivation_id: "01990f5f-55c0-7000-8000-000000000001",
            input_digests: [digest],
            output_digests: [`sha256:${"b2".repeat(32)}`],
            transformation_name: "signalbox.image.thumbnail",
            transformation_version: 1,
            parameters_json: '{"max_edge":256}',
            producer: {
              class: "deterministic",
              implementation_digest: `sha256:${"c3".repeat(32)}`,
              cache_key: `sha256:${"d4".repeat(32)}`,
            },
          },
        ],
      },
    ],
  });

  assert.equal(descriptor.available_views[0].kind, "thumbnail");
  assert.equal(descriptor.byte_length, "94371840");
});

test("generated blob decoder rejects off-origin content URLs", () => {
  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        digest: `sha256:${"a1".repeat(32)}`,
        byte_length: "1",
        declared_media_type: "application/octet-stream",
        display_filename: [],
        available_views: [
          {
            kind: "download",
            content_url: "https://example.invalid/blob",
            media_type: "application/octet-stream",
            byte_length: "1",
            derivations: [],
          },
        ],
      }),
    /same-origin blob API path/,
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
