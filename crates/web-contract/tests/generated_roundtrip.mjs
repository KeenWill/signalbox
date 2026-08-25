import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  decodeWebApiErrorResponse,
  decodeWebAttentionSnapshot,
  decodeWebAttentionStreamEvent,
  decodeWebBlobDescriptor,
  decodeWebContractBootstrap,
  decodeWebContractExample,
  decodeWebImportListPage,
  decodeWebSearchPage,
  decodeWebSessionTimelineDescriptor,
  decodeWebSessionTimelineWindow,
  decodeWebUsageCallPage,
  decodeWebUsageSummary,
} from "../../../clients/web/src/generated/web-contract.mjs";

const fixtureUrl = new URL("./fixtures/example.json", import.meta.url);

function searchPage() {
  return {
    results: [
      {
        session_id: "00000000-0000-0000-0000-000000000991",
        address: { event_sequence: "1" },
        projection_id: "1",
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

function assertCanonicalNumberSpelling(parameters_json) {
  const digest = `sha256:${"a1".repeat(32)}`;

  assert.throws(
    () => decodeWebBlobDescriptor({
      digest,
      byte_length: "1",
      declared_media_type: "image/png",
      display_filename: [],
      available_views: [
        {
          kind: "download",
          content_url: `/api/blobs/${digest}/download?media_type=image%2Fpng`,
          media_type: "image/png",
          byte_length: "1",
          derivations: [],
        },
        {
          kind: "preview",
          content_url: `/api/blobs/${digest}/content/image-png`,
          media_type: "image/png",
          byte_length: "1",
          derivations: [
            {
              derivation_id: "01990f5f-55c0-7000-8000-000000000001",
              input_digests: [digest],
              output_digests: [digest],
              transformation_name: "image.preview",
              transformation_version: 1,
              parameters_json,
              producer: {
                class: "executed",
                execution_id: "01990f5f-55c0-7000-8000-000000000002",
                implementation_digest: digest,
              },
            },
          ],
        },
      ],
    }),
    /derivations must be the exact deterministic image transformation/,
  );
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
        contract: { name: "signalbox.web-http", version: "999" },
        capabilities: {
          bounded_json: true,
          bounded_lexical_search: true,
          same_origin_json_mutations: true,
          ndjson_streaming: true,
          immutable_blob_content: true,
          blob_derivations: true,
          image_derivatives: true,
          import_discovery: true,
          imported_continuations: true,
          bounded_session_timeline: true,
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

test("generated bootstrap decoder rejects a disabled required capability", () => {
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
          immutable_blob_content: true,
          blob_derivations: true,
          image_derivatives: true,
          import_discovery: false,
          imported_continuations: true,
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

test("generated blob decoder accepts capability-projected views", () => {
  const digest = "sha256:3729b2319da081a0710ba27da7af330c1236325cf8ed0a619cf132375bb0fc1e";
  const outputDigest = "sha256:071d25f582ba9e6a8725e198dab884d70a3d7ce3ea84a74c66e65a1443c41a8e";
  const descriptor = decodeWebBlobDescriptor({
    digest,
    byte_length: "94371840",
    declared_media_type: "image/png",
    display_filename: ["capture.png"],
    available_views: [
      {
        kind: "download",
        content_url: `/api/blobs/${digest}/download?media_type=image%2Fpng&display_filename=capture.png`,
        media_type: "image/png",
        byte_length: "94371840",
        derivations: [],
      },
      {
        kind: "preview",
        content_url: `/api/blobs/${outputDigest}/content/image-png`,
        media_type: "image/png",
        byte_length: "2048",
        derivations: [
          {
            derivation_id: "01990f5f-55c0-7000-8000-000000000001",
            input_digests: [digest],
            output_digests: [outputDigest],
            transformation_name: "image.preview",
            transformation_version: 1,
            parameters_json: '{"edge_px":1600,"format":"image/png"}',
            producer: {
              class: "deterministic",
              implementation_digest: `sha256:${"4d".repeat(32)}`,
              cache_key: "sha256:07257dcebadabd8928bfae61ebcf7c45ead3d35cf94cfdacf572f40695668816",
            },
          },
        ],
      },
    ],
  });

  assert.equal(descriptor.available_views[1].kind, "preview");
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
    /root-relative blob API path/,
  );
});

test("generated blob decoder rejects invalid download media metadata", () => {
  const digest = `sha256:${"a1".repeat(32)}`;

  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        digest,
        byte_length: "1",
        declared_media_type: "application/octet-stream",
        display_filename: [],
        available_views: [
          {
            kind: "download",
            content_url: `/api/blobs/${digest}/download?media_type=%00`,
            media_type: "application/octet-stream",
            byte_length: "1",
            derivations: [],
          },
        ],
      }),
    /MIME value/,
  );
});

test("generated blob decoder rejects daemon-invalid quoted MIME values", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const descriptor = {
    digest,
    byte_length: "1",
    declared_media_type: "application/octet-stream",
    display_filename: [],
    available_views: [
      {
        kind: "download",
        content_url: `/api/blobs/${digest}/download?media_type=text%2Fplain%3Bfoo%3D%22%00%22`,
        media_type: "application/octet-stream",
        byte_length: "1",
        derivations: [],
      },
    ],
  };

  assert.throws(() => decodeWebBlobDescriptor(descriptor), /MIME value/);
  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        ...descriptor,
        available_views: [
          {
            ...descriptor.available_views[0],
            content_url: `/api/blobs/${digest}/download?media_type=text%2Fplain%3Bfoo%3D%22a%5C%22b%22`,
          },
        ],
      }),
    /MIME value/,
  );
});

test("generated blob decoder bounds declared media types", () => {
  const digest = `sha256:${"a1".repeat(32)}`;

  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        digest,
        byte_length: "1",
        declared_media_type: `text/${"x".repeat(251)}`,
        display_filename: [],
        available_views: [
          {
            kind: "download",
            content_url: `/api/blobs/${digest}/download?media_type=application%2Foctet-stream`,
            media_type: "application/octet-stream",
            byte_length: "1",
            derivations: [],
          },
        ],
      }),
    /declared_media_type must be/,
  );
});

test("generated blob decoder rejects multiple display filenames", () => {
  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        digest: `sha256:${"a1".repeat(32)}`,
        byte_length: "1",
        declared_media_type: "application/octet-stream",
        display_filename: ["first.bin", "second.bin"],
        available_views: [],
      }),
    /display_filename must be at most 1 items/,
  );
});

test("generated blob decoder rejects a zero transformation version", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        digest,
        byte_length: "1",
        declared_media_type: "image/png",
        display_filename: [],
        available_views: [
          {
            kind: "preview",
            content_url: `/api/blobs/${digest}/content/image-png`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [
              {
                derivation_id: "01990f5f-55c0-7000-8000-000000000001",
                input_digests: [digest],
                output_digests: [digest],
                transformation_name: "image.preview",
                transformation_version: 0,
                parameters_json: "{}",
                producer: {
                  class: "deterministic",
                  implementation_digest: digest,
                  cache_key: digest,
                },
              },
            ],
          },
        ],
      }),
    /transformation_version must be at least 1/,
  );
});

test("generated blob decoder rejects invalid derivation digest cardinality", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const derivation = {
    derivation_id: "01990f5f-55c0-7000-8000-000000000001",
    input_digests: [],
    output_digests: Array.from({ length: 17 }, () => digest),
    transformation_name: "image.preview",
    transformation_version: 1,
    parameters_json: "{}",
    producer: {
      class: "deterministic",
      implementation_digest: digest,
      cache_key: digest,
    },
  };
  const descriptor = {
    digest,
    byte_length: "1",
    declared_media_type: "image/png",
    display_filename: [],
    available_views: [
      {
        kind: "preview",
        content_url: `/api/blobs/${digest}/content/image-png`,
        media_type: "image/png",
        byte_length: "1",
        derivations: [derivation],
      },
    ],
  };

  assert.throws(
    () => decodeWebBlobDescriptor(descriptor),
    /input_digests must be at least 1 items/,
  );
  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        ...descriptor,
        available_views: [
          {
            ...descriptor.available_views[0],
            derivations: [{ ...derivation, input_digests: [digest] }],
          },
        ],
      }),
    /output_digests must be at most 16 items/,
  );
});

test("generated blob decoder rejects an invalid transformation name", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        digest,
        byte_length: "1",
        declared_media_type: "image/png",
        display_filename: [],
        available_views: [
          {
            kind: "preview",
            content_url: `/api/blobs/${digest}/content/image-png`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [
              {
                derivation_id: "01990f5f-55c0-7000-8000-000000000001",
                input_digests: [digest],
                output_digests: [digest],
                transformation_name: "Image.Preview",
                transformation_version: 1,
                parameters_json: "{}",
                producer: {
                  class: "deterministic",
                  implementation_digest: digest,
                  cache_key: digest,
                },
              },
            ],
          },
        ],
      }),
    /transformation_name must be matching/,
  );
});

test("generated blob decoder rejects an absolute sentinel-origin URL", () => {
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
            content_url: `http://signalbox.invalid/api/blobs/sha256:${"a1".repeat(32)}/download`,
            media_type: "application/octet-stream",
            byte_length: "1",
            derivations: [],
          },
        ],
      }),
    /root-relative blob API path/,
  );
});

test("generated blob decoder rejects noncanonical transformation parameters", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        digest,
        byte_length: "1",
        declared_media_type: "image/png",
        display_filename: [],
        available_views: [
          {
            kind: "download",
            content_url: `/api/blobs/${digest}/download?media_type=image%2Fpng`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [],
          },
          {
            kind: "preview",
            content_url: `/api/blobs/${digest}/content/image-png`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [
              {
                derivation_id: "01990f5f-55c0-7000-8000-000000000001",
                input_digests: [digest],
                output_digests: [digest],
                transformation_name: "image.preview",
                transformation_version: 1,
                parameters_json: '{"z":1,"a":2}',
                producer: {
                  class: "deterministic",
                  implementation_digest: digest,
                  cache_key: digest,
                },
              },
            ],
          },
        ],
      }),
    /parameters_json must be canonical JSON/,
  );
});

test("generated blob decoder rejects lone surrogates in canonical parameters", () => {
  const digest = `sha256:${"a1".repeat(32)}`;

  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        digest,
        byte_length: "1",
        declared_media_type: "image/png",
        display_filename: [],
        available_views: [
          {
            kind: "download",
            content_url: `/api/blobs/${digest}/download?media_type=image%2Fpng`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [],
          },
          {
            kind: "preview",
            content_url: `/api/blobs/${digest}/content/image-png`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [
              {
                derivation_id: "01990f5f-55c0-7000-8000-000000000001",
                input_digests: [digest],
                output_digests: [digest],
                transformation_name: "image.preview",
                transformation_version: 1,
                parameters_json: String.raw`"\ud800"`,
                producer: {
                  class: "executed",
                  execution_id: "01990f5f-55c0-7000-8000-000000000002",
                  implementation_digest: digest,
                },
              },
            ],
          },
        ],
      }),
    /parameters_json must be canonical JSON/,
  );
});

test("generated blob decoder accepts exact large JSON integers", () => {
  assertCanonicalNumberSpelling('{"n":9007199254740993}');
});

test("generated blob decoder rejects invalid display filenames", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const descriptor = {
    digest,
    byte_length: "1",
    declared_media_type: "application/octet-stream",
    display_filename: [""],
    available_views: [
      {
        kind: "download",
        content_url: `/api/blobs/${digest}/download?media_type=application%2Foctet-stream`,
        media_type: "application/octet-stream",
        byte_length: "1",
        derivations: [],
      },
    ],
  };

  assert.throws(
    () => decodeWebBlobDescriptor(descriptor),
    /display_filename\[0\] must be a nonempty/,
  );
  assert.throws(
    () => decodeWebBlobDescriptor({ ...descriptor, display_filename: ["bad\nname"] }),
    /display_filename\[0\] must be a nonempty/,
  );
  assert.throws(
    () => decodeWebBlobDescriptor({ ...descriptor, display_filename: ["é".repeat(513)] }),
    /display_filename\[0\] must be a nonempty/,
  );
});

test("generated blob decoder requires exactly one download view", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const download = {
    kind: "download",
    content_url: `/api/blobs/${digest}/download?media_type=application%2Foctet-stream`,
    media_type: "application/octet-stream",
    byte_length: "1",
    derivations: [],
  };
  const descriptor = {
    digest,
    byte_length: "1",
    declared_media_type: "application/octet-stream",
    display_filename: [],
    available_views: [],
  };

  assert.throws(
    () => decodeWebBlobDescriptor(descriptor),
    /available_views must be exactly one download view/,
  );
  assert.throws(
    () => decodeWebBlobDescriptor({ ...descriptor, available_views: [download, download] }),
    /available_views must be exactly one download view/,
  );
});

test("generated blob decoder rejects zero-length descriptors and views", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const download = {
    kind: "download",
    content_url: `/api/blobs/${digest}/download?media_type=application%2Foctet-stream`,
    media_type: "application/octet-stream",
    byte_length: "1",
    derivations: [],
  };
  const descriptor = {
    digest,
    byte_length: "0",
    declared_media_type: "application/octet-stream",
    display_filename: [],
    available_views: [download],
  };

  assert.throws(
    () => decodeWebBlobDescriptor(descriptor),
    /byte_length must be a positive canonical/,
  );
  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        ...descriptor,
        byte_length: "1",
        available_views: [{ ...download, byte_length: "0" }],
      }),
    /byte_length must be a positive canonical/,
  );
});

test("generated blob decoder ties content routes to advertised digests", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const otherDigest = `sha256:${"b2".repeat(32)}`;
  const descriptor = {
    digest,
    byte_length: "1",
    declared_media_type: "application/octet-stream",
    display_filename: [],
    available_views: [
      {
        kind: "download",
        content_url: `/api/blobs/${otherDigest}/download?media_type=application%2Foctet-stream`,
        media_type: "application/octet-stream",
        byte_length: "1",
        derivations: [],
      },
    ],
  };

  assert.throws(
    () => decodeWebBlobDescriptor(descriptor),
    /content_url must be a route for the descriptor digest/,
  );
});

test("generated blob decoder rejects provenance on original views", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const derivation = {
    derivation_id: "01990f5f-55c0-7000-8000-000000000001",
    input_digests: [digest],
    output_digests: [digest],
    transformation_name: "image.preview",
    transformation_version: 1,
    parameters_json: "{}",
    producer: {
      class: "executed",
      execution_id: "01990f5f-55c0-7000-8000-000000000002",
      implementation_digest: digest,
    },
  };

  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        digest,
        byte_length: "1",
        declared_media_type: "image/png",
        display_filename: [],
        available_views: [
          {
            kind: "download",
            content_url: `/api/blobs/${digest}/download?media_type=image%2Fpng`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [],
          },
          {
            kind: "browser_native",
            content_url: `/api/blobs/${digest}/content/image-png`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [derivation],
          },
        ],
      }),
    /derivations must be empty for an original representation/,
  );
});

test("generated blob decoder binds derivative provenance to the descriptor input", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const unrelatedDigest = `sha256:${"b2".repeat(32)}`;
  const outputDigest = `sha256:${"c3".repeat(32)}`;
  const descriptor = {
    digest,
    byte_length: "1",
    declared_media_type: "image/png",
    display_filename: [],
    available_views: [
      {
        kind: "download",
        content_url: `/api/blobs/${digest}/download?media_type=image%2Fpng`,
        media_type: "image/png",
        byte_length: "1",
        derivations: [],
      },
      {
        kind: "preview",
        content_url: `/api/blobs/${outputDigest}/content/image-png`,
        media_type: "image/png",
        byte_length: "1",
        derivations: [
          {
            derivation_id: "01990f5f-55c0-7000-8000-000000000001",
            input_digests: [unrelatedDigest],
            output_digests: [outputDigest],
            transformation_name: "image.preview",
            transformation_version: 1,
            parameters_json: "{}",
            producer: {
              class: "deterministic",
              implementation_digest: digest,
              cache_key: digest,
            },
          },
        ],
      },
    ],
  };

  assert.throws(
    () => decodeWebBlobDescriptor(descriptor),
    /content_url must be a route for a derivation output bound to the descriptor input/,
  );
});

test("generated blob decoder bounds nested view collections", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const download = {
    kind: "download",
    content_url: `/api/blobs/${digest}/download?media_type=application%2Foctet-stream`,
    media_type: "application/octet-stream",
    byte_length: "1",
    derivations: [],
  };
  const descriptor = {
    digest,
    byte_length: "1",
    declared_media_type: "application/octet-stream",
    display_filename: [],
    available_views: Array.from({ length: 5 }, () => download),
  };

  assert.throws(
    () => decodeWebBlobDescriptor(descriptor),
    /available_views must be at most 4 items/,
  );

  const derivation = {
    derivation_id: "01990f5f-55c0-7000-8000-000000000001",
    input_digests: [digest],
    output_digests: [digest],
    transformation_name: "image.preview",
    transformation_version: 1,
    parameters_json: "{}",
    producer: {
      class: "deterministic",
      implementation_digest: digest,
      cache_key: digest,
    },
  };
  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        ...descriptor,
        available_views: [
          download,
          {
            kind: "preview",
            content_url: `/api/blobs/${digest}/content/image-png`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [derivation, derivation],
          },
        ],
      }),
    /derivations must be at most 1 items/,
  );
});

test("generated blob decoder accepts Rust large-exponent spelling", () => {
  assertCanonicalNumberSpelling('{"n":1e+20}');
});

test("generated blob decoder accepts Rust small-exponent spelling", () => {
  assertCanonicalNumberSpelling('{"n":1e-6}');
});

test("generated blob decoder accepts Rust negative-zero spelling", () => {
  assertCanonicalNumberSpelling('{"n":-0.0}');
});

test("generated blob decoder accepts arbitrary-precision decimal spelling", () => {
  assertCanonicalNumberSpelling('{"n":1.00}');
});

test("generated blob decoder accepts arbitrary-precision integer spelling", () => {
  assertCanonicalNumberSpelling('{"n":18446744073709551616}');
});

test("generated blob decoder accepts arbitrary-precision exponent spelling", () => {
  assertCanonicalNumberSpelling('{"n":1e+999}');
});

test("generated blob decoder binds original metadata to the descriptor", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const download = {
    kind: "download",
    content_url: `/api/blobs/${digest}/download?media_type=image%2Fpng`,
    media_type: "image/png",
    byte_length: "999",
    derivations: [],
  };
  const descriptor = {
    digest,
    byte_length: "10",
    declared_media_type: "image/png",
    display_filename: [],
    available_views: [download],
  };

  assert.throws(
    () => decodeWebBlobDescriptor(descriptor),
    /byte_length must be the descriptor byte length for an original representation/,
  );
  assert.throws(
    () => decodeWebBlobDescriptor({
      ...descriptor,
      declared_media_type: "text/plain",
      available_views: [{ ...download, byte_length: "10" }],
    }),
    /media_type must be the descriptor declared media type for the download representation/,
  );
});

test("generated blob decoder binds browser-native routes to the declared image type", () => {
  const digest = `sha256:${"a1".repeat(32)}`;

  assert.throws(
    () => decodeWebBlobDescriptor({
      digest,
      byte_length: "1",
      declared_media_type: "image/jpeg",
      display_filename: [],
      available_views: [
        {
          kind: "download",
          content_url: `/api/blobs/${digest}/download?media_type=image%2Fjpeg`,
          media_type: "image/jpeg",
          byte_length: "1",
          derivations: [],
        },
        {
          kind: "browser_native",
          content_url: `/api/blobs/${digest}/content/image-png`,
          media_type: "image/png",
          byte_length: "1",
          derivations: [],
        },
      ],
    }),
    /content_url must be an original-image route matching the descriptor declared media type/,
  );
});

test("generated blob decoder rejects duplicate representation kinds", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const browserNative = {
    kind: "browser_native",
    content_url: `/api/blobs/${digest}/content/image-png`,
    media_type: "image/png",
    byte_length: "1",
    derivations: [],
  };

  assert.throws(
    () => decodeWebBlobDescriptor({
      digest,
      byte_length: "1",
      declared_media_type: "image/png",
      display_filename: [],
      available_views: [
        {
          kind: "download",
          content_url: `/api/blobs/${digest}/download?media_type=image%2Fpng`,
          media_type: "image/png",
          byte_length: "1",
          derivations: [],
        },
        browserNative,
        { ...browserNative },
      ],
    }),
    /available_views must be at most one view of each representation kind/,
  );
});

test("generated blob decoder binds view kinds to exact image transformations", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const outputDigest = `sha256:${"b2".repeat(32)}`;

  assert.throws(
    () => decodeWebBlobDescriptor({
      digest,
      byte_length: "1",
      declared_media_type: "image/png",
      display_filename: [],
      available_views: [
        {
          kind: "download",
          content_url: `/api/blobs/${digest}/download?media_type=image%2Fpng`,
          media_type: "image/png",
          byte_length: "1",
          derivations: [],
        },
        {
          kind: "preview",
          content_url: `/api/blobs/${outputDigest}/content/image-png`,
          media_type: "image/png",
          byte_length: "1",
          derivations: [{
            derivation_id: "01990f5f-55c0-7000-8000-000000000001",
            input_digests: [digest],
            output_digests: [outputDigest],
            transformation_name: "totally.unrelated",
            transformation_version: 99,
            parameters_json: "{}",
            producer: {
              class: "executed",
              execution_id: "01990f5f-55c0-7000-8000-000000000002",
              implementation_digest: digest,
            },
          }],
        },
      ],
    }),
    /derivations must be the exact deterministic image transformation/,
  );
});

test("generated blob decoder rejects download routes for derivative views", () => {
  const digest = `sha256:${"a1".repeat(32)}`;

  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        digest,
        byte_length: "1",
        declared_media_type: "image/png",
        display_filename: [],
        available_views: [
          {
            kind: "download",
            content_url: `/api/blobs/${digest}/download?media_type=image%2Fpng`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [],
          },
          {
            kind: "preview",
            content_url: `/api/blobs/${digest}/download?media_type=image%2Fpng`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [
              {
                derivation_id: "01990f5f-55c0-7000-8000-000000000001",
                input_digests: [digest],
                output_digests: [digest],
                transformation_name: "image.preview",
                transformation_version: 1,
                parameters_json: "{}",
                producer: {
                  class: "executed",
                  execution_id: "01990f5f-55c0-7000-8000-000000000002",
                  implementation_digest: digest,
                },
              },
            ],
          },
        ],
      }),
    /content_url must be an image-content route for a derivative view/,
  );
});

test("generated blob decoder rejects unsupported routes and incomplete download metadata", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const descriptor = {
    digest,
    byte_length: "1",
    declared_media_type: "application/octet-stream",
    display_filename: [],
    available_views: [
      {
        kind: "download",
        content_url: `/api/blobs/${digest}/download`,
        media_type: "application/octet-stream",
        byte_length: "1",
        derivations: [],
      },
    ],
  };

  assert.throws(
    () => decodeWebBlobDescriptor(descriptor),
    /download route with required media type metadata/,
  );
  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        ...descriptor,
        available_views: [
          {
            ...descriptor.available_views[0],
            kind: "browser_native",
            content_url: `/api/blobs/${digest}/content/arbitrary-name`,
          },
        ],
      }),
    /canonical blob API route/,
  );
});

test("generated blob decoder binds download filenames to descriptor metadata", () => {
  const digest = `sha256:${"a1".repeat(32)}`;

  assert.throws(
    () => decodeWebBlobDescriptor({
      digest,
      byte_length: "1",
      declared_media_type: "application/pdf",
      display_filename: ["report.pdf"],
      available_views: [{
        kind: "download",
        content_url: `/api/blobs/${digest}/download?media_type=application%2Fpdf&display_filename=payload.exe`,
        media_type: "application/pdf",
        byte_length: "1",
        derivations: [],
      }],
    }),
    /content_url must be download filename metadata matching the descriptor/,
  );
});

test("generated blob decoder cross-checks view and route media types", () => {
  const routeDigest = `sha256:${"a1".repeat(32)}`;
  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        digest: routeDigest,
        byte_length: "1",
        declared_media_type: "image/png",
        display_filename: [],
        available_views: [
          {
            kind: "download",
            content_url: `/api/blobs/${routeDigest}/download?media_type=image%2Fpng`,
            media_type: "text/plain",
            byte_length: "1",
            derivations: [],
          },
        ],
      }),
    /media_type must be the content route media type/,
  );
});

test("generated blob decoder rejects a contradictory deterministic cache key", () => {
  const digest = `sha256:${"a1".repeat(32)}`;
  const outputDigest = `sha256:${"b2".repeat(32)}`;

  assert.throws(
    () =>
      decodeWebBlobDescriptor({
        digest,
        byte_length: "1",
        declared_media_type: "image/png",
        display_filename: [],
        available_views: [
          {
            kind: "download",
            content_url: `/api/blobs/${digest}/download?media_type=image%2Fpng`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [],
          },
          {
            kind: "preview",
            content_url: `/api/blobs/${outputDigest}/content/image-png`,
            media_type: "image/png",
            byte_length: "1",
            derivations: [
              {
                derivation_id: "01990f5f-55c0-7000-8000-000000000001",
                input_digests: [digest],
                output_digests: [outputDigest],
                transformation_name: "image.preview",
                transformation_version: 1,
                parameters_json: "{}",
                producer: {
                  class: "deterministic",
                  implementation_digest: digest,
                  cache_key: outputDigest,
                },
              },
            ],
          },
        ],
      }),
    /cache_key must be the deterministic key for the advertised provenance/,
  );
});

test("generated bootstrap decoder rejects incompatible limits", () => {
  assert.throws(
    () =>
      decodeWebContractBootstrap({
        contract: { name: "signalbox.web-http", version: "2" },
        capabilities: {
          bounded_json: true,
          bounded_lexical_search: true,
          same_origin_json_mutations: true,
          ndjson_streaming: true,
          immutable_blob_content: true,
          blob_derivations: true,
          image_derivatives: true,
          import_discovery: true,
          imported_continuations: true,
          bounded_session_timeline: true,
        },
        limits: {
          max_json_body_bytes: 1,
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

test("generated imports decoder validates explicit null evidence and cursor", () => {
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

  const decoded = decodeWebImportListPage(page);

  assert.equal(decoded.items[0].source_session_id, null);
  assert.equal(decoded.next_cursor, null);
  assert.throws(
    () =>
      decodeWebImportListPage({
        ...page,
        items: [{ ...page.items[0], entry_count: "1" }],
      }),
    /entry_count must be a safe integer/,
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

test("generated usage decoder preserves nullable axes and labeled cost", () => {
  const summary = decodeWebUsageSummary({
    groups: [
      {
        call_kind: "model_call",
        model_id: "00000000-0000-0000-0000-000000000041",
        profile_id: "fixture-primary",
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
    profile_id: "fixture-primary",
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
    profile_id: "fixture-primary",
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
    }, "newest"),
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

test("generated usage decoder requires positive summary call counts", () => {
  const group = usageGroup();
  group.call_count = "0";

  assert.throws(
    () => decodeWebUsageSummary({ groups: [group], truncated: false }),
    /matching|positive/,
  );
});

test("generated usage decoder caps summary call counts at the aggregation ceiling", () => {
  const group = usageGroup();
  group.call_count = "10001";

  assert.throws(
    () => decodeWebUsageSummary({ groups: [group], truncated: false }),
    /matching/,
  );
});

test("generated usage decoder constrains call and cursor timestamps", () => {
  const call = usageCall();
  call.recorded_at_micros = "253402300800000000";
  assert.throws(
    () => decodeWebUsageCallPage({ calls: [call], continuation: null }, "newest"),
    /application-range usage timestamp|matching|one recognized variant/,
  );

  const validCall = usageCall();
  const continuation = {
    recorded_at_micros: "253402300800000000",
    call_id: validCall.call_id,
  };
  assert.throws(
    () => decodeWebUsageCallPage({ calls: [validCall], continuation }, "newest"),
    /application-range usage timestamp|matching|one recognized variant/,
  );
});

test("generated usage decoder bounds rate versions by UTF-8 bytes", () => {
  const empty = usageGroup();
  empty.cost.rate_version = "";
  const oversized = usageGroup();
  oversized.cost.rate_version = "é".repeat(65);

  assert.throws(
    () => decodeWebUsageSummary({ groups: [empty], truncated: false }),
    /at least 1 characters|1 through 128 UTF-8 bytes/,
  );
  assert.throws(
    () => decodeWebUsageSummary({ groups: [oversized], truncated: false }),
    /1 through 128 UTF-8 bytes/,
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
    /unavailable with reason no_token_evidence/,
  );
  assert.throws(
    () => decodeWebUsageCallPage({ calls: [call], continuation: null }, "newest"),
    /unavailable with reason no_token_evidence/,
  );
});

test("generated usage decoder rejects malformed identities", () => {
  const call = usageCall();
  call.call_id = "not-a-uuid";

  assert.throws(
    () => decodeWebUsageCallPage({ calls: [call], continuation: null }, "newest"),
    /matching/,
  );
});

test("generated usage decoder validates ordering and cursor correlation", () => {
  const first = usageCall();
  const second = usageCall();
  second.call_id = "00000000-0000-0000-0000-000000000050";
  second.recorded_at_micros = "1777777777123455";
  const page = {
    calls: [first, second],
    continuation: {
      recorded_at_micros: second.recorded_at_micros,
      call_id: second.call_id,
    },
  };

  assert.equal(decodeWebUsageCallPage(page, "newest"), page);
  assert.throws(
    () => decodeWebUsageCallPage({ ...page, calls: [second, first] }, "newest"),
    /strictly descending by call key/,
  );
  assert.throws(
    () =>
      decodeWebUsageCallPage(
        {
          ...page,
          continuation: { ...page.continuation, call_id: first.call_id },
        },
        "newest",
      ),
    /cursor anchored to the final usage call/,
  );
});

test("generated usage decoder accepts an omitted optional continuation", () => {
  const page = { calls: [usageCall()] };

  assert.equal(decodeWebUsageCallPage(page, "newest"), page);
});

test("generated usage decoder rejects repeated call identities", () => {
  const first = usageCall();
  const repeated = usageCall();
  repeated.recorded_at_micros = "1777777777123455";

  assert.throws(
    () => decodeWebUsageCallPage(
      { calls: [first, repeated], continuation: null },
      "newest",
    ),
    /unique within the page/,
  );
});

test("generated usage decoder rejects spurious invalid cache breakdowns", () => {
  const call = usageCall();
  call.cost = {
    status: "unavailable",
    reason: "invalid_cache_breakdown",
  };

  assert.throws(
    () => decodeWebUsageCallPage({ calls: [call], continuation: null }, "newest"),
    /consistent with token evidence and input semantics/,
  );
});

test("generated usage summary preserves hidden constituent breakdown failures", () => {
  const group = usageGroup();
  group.call_count = "2";
  group.input_semantics = "cache_inclusive";
  group.coverage.cache_creation_input = true;
  group.coverage.cache_read_input = true;
  group.tokens.input = "101";
  group.tokens.cache_creation_input = "2";
  group.tokens.cache_read_input = "0";
  group.cost = { status: "unavailable", reason: "invalid_cache_breakdown" };

  assert.equal(
    decodeWebUsageSummary({ groups: [group], truncated: false }).groups[0],
    group,
  );
});

test("generated usage summary rejects hidden breakdown failures for singletons", () => {
  const group = usageGroup();
  group.input_semantics = "cache_inclusive";
  group.coverage.cache_creation_input = true;
  group.coverage.cache_read_input = true;
  group.tokens.cache_creation_input = "2";
  group.tokens.cache_read_input = "0";
  group.cost = { status: "unavailable", reason: "invalid_cache_breakdown" };

  assert.throws(
    () => decodeWebUsageSummary({ groups: [group], truncated: false }),
    /consistent with token evidence and input semantics/,
  );
});

test("generated usage summary rejects hidden breakdown failures missing a cache axis", () => {
  const group = usageGroup();
  group.call_count = "2";
  group.input_semantics = "cache_inclusive";
  group.coverage.cache_read_input = true;
  group.tokens.input = "101";
  group.tokens.cache_read_input = "0";
  group.cost = { status: "unavailable", reason: "invalid_cache_breakdown" };

  assert.throws(
    () => decodeWebUsageSummary({ groups: [group], truncated: false }),
    /consistent with token evidence and input semantics/,
  );
});

test("generated usage summary bounds totals by represented calls", () => {
  const group = usageGroup();
  group.tokens.input = "18446744073709551616";

  assert.throws(
    () => decodeWebUsageSummary({ groups: [group], truncated: false }),
    /bounded by call_count times u64::MAX/,
  );
});

test("generated usage summary rejects duplicate compatibility keys", () => {
  const first = usageGroup();
  const duplicate = structuredClone(first);
  duplicate.call_count = "2";

  assert.throws(
    () => decodeWebUsageSummary({ groups: [first, duplicate], truncated: false }),
    /unique compatibility key/,
  );
});

test("generated usage summary caps represented calls across groups", () => {
  const first = usageGroup();
  first.call_count = "10000";
  const second = usageGroup();
  second.profile_id = "fixture-secondary";

  assert.throws(
    () => decodeWebUsageSummary({ groups: [first, second], truncated: false }),
    /at most 10000 represented calls/,
  );
});

test("generated usage summary rejects hidden breakdown failures for exclusive input", () => {
  const group = usageGroup();
  group.cost = { status: "unavailable", reason: "invalid_cache_breakdown" };

  assert.throws(
    () => decodeWebUsageSummary({ groups: [group], truncated: false }),
    /consistent with token evidence and input semantics/,
  );
});

test("generated usage decoder bounds profile identities by UTF-8 bytes", () => {
  const empty = usageGroup();
  empty.profile_id = "";
  const oversized = usageGroup();
  oversized.profile_id = "é".repeat(129);

  assert.throws(
    () => decodeWebUsageSummary({ groups: [empty], truncated: false }),
    /at least 1 characters|1 through 256 UTF-8 bytes/,
  );
  assert.throws(
    () => decodeWebUsageSummary({ groups: [oversized], truncated: false }),
    /1 through 256 UTF-8 bytes/,
  );
});

test("generated usage decoder bounds call profile identities by UTF-8 bytes", () => {
  const oversized = usageCall();
  oversized.profile_id = "é".repeat(129);

  assert.throws(
    () => decodeWebUsageCallPage({ calls: [oversized], continuation: null }, "newest"),
    /1 through 256 UTF-8 bytes/,
  );
});

test("generated usage decoder correlates call kind with turn presence", () => {
  const compaction = usageCall();
  compaction.call_kind = "context_compaction";
  const ordinary = usageCall();
  ordinary.turn_id = null;

  assert.throws(
    () => decodeWebUsageCallPage({ calls: [compaction], continuation: null }, "newest"),
    /turn_id must be string|null exactly for context compaction calls/,
  );
  assert.throws(
    () => decodeWebUsageCallPage({ calls: [ordinary], continuation: null }, "newest"),
    /turn_id must be string|null exactly for context compaction calls/,
  );
});

test("generated usage decoder rejects omitted turns for turn-scoped calls", () => {
  const ordinary = usageCall();
  delete ordinary.turn_id;

  assert.throws(
    () => decodeWebUsageCallPage({ calls: [ordinary], continuation: null }, "newest"),
    /turn_id.*present|null exactly for context compaction calls/,
  );
});

test("generated usage decoders reject cost states inconsistent with evidence", () => {
  const unknownSemantics = usageCall();
  unknownSemantics.input_semantics = "unknown";
  assert.throws(
    () =>
      decodeWebUsageCallPage(
        { calls: [unknownSemantics], continuation: null },
        "newest",
      ),
    /unavailable with reason unknown_input_semantics/,
  );

  const contradictoryUnavailable = usageGroup();
  contradictoryUnavailable.cost = {
    status: "unavailable",
    reason: "no_token_evidence",
  };
  assert.throws(
    () =>
      decodeWebUsageSummary({
        groups: [contradictoryUnavailable],
        truncated: false,
      }),
    /consistent with token evidence and input semantics/,
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

test("generated search decoder rejects too many highlight ranges", () => {
  const page = searchPage();
  page.results[0].snippet = "x".repeat(512);
  page.results[0].highlights = Array.from({ length: 513 }, () => ({
    start_byte: 0,
    end_byte: 1,
  }));

  assert.throws(
    () => decodeWebSearchPage(page),
    /highlights must be at most 512 items/,
  );
});

test("generated search decoder rejects continuation on an empty page", () => {
  const empty = searchPage();
  empty.results = [];
  assert.throws(
    () => decodeWebSearchPage(empty),
    /cursor anchored to the final search result/,
  );
});

test("generated search decoder rejects a continuation address mismatch", () => {
  const mismatched = searchPage();
  mismatched.continuation.address.event_sequence = "2";
  assert.throws(
    () => decodeWebSearchPage(mismatched),
    /cursor anchored to the final search result/,
  );
});

test("generated search decoder rejects a continuation projection mismatch", () => {
  const mismatchedProjection = searchPage();
  mismatchedProjection.continuation.projection_id = "2";
  assert.throws(
    () => decodeWebSearchPage(mismatchedProjection),
    /cursor anchored to the final search result/,
  );
});

test("generated search decoder rejects out-of-order result addresses", () => {
  const page = searchPage();
  const newer = structuredClone(page.results[0]);
  newer.address.event_sequence = "2";
  newer.projection_id = "2";
  page.results.push(newer);
  page.continuation.address.event_sequence = "2";
  page.continuation.projection_id = "2";

  assert.throws(
    () => decodeWebSearchPage(page),
    /strictly descending search result key/,
  );
});

test("generated search decoder rejects out-of-order same-address projections", () => {
  const page = searchPage();
  page.results[0].projection_id = "2";
  const outOfOrder = structuredClone(page.results[0]);
  outOfOrder.projection_id = "3";
  page.results.push(outOfOrder);
  page.continuation.projection_id = "3";

  assert.throws(
    () => decodeWebSearchPage(page),
    /strictly descending search result key/,
  );
});

test("generated search decoder rejects malformed result identities", () => {
  const page = searchPage();
  page.results[0].session_id = "not-a-uuid";

  assert.throws(() => decodeWebSearchPage(page), /matching/);
});

test("generated search decoder rejects a contradictory source session", () => {
  const mismatchedSession = searchPage();
  mismatchedSession.results[0].source.session_id =
    "00000000-0000-0000-0000-000000000992";
  assert.throws(
    () => decodeWebSearchPage(mismatchedSession),
    /source consistent with the result session and content class/,
  );
});

test("generated search decoder rejects a contradictory content class", () => {
  const mismatchedContent = searchPage();
  mismatchedContent.results[0].content_class = "tool_result";
  assert.throws(
    () => decodeWebSearchPage(mismatchedContent),
    /source consistent with the result session and content class/,
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
