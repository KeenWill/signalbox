import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  decodeWebApiErrorResponse,
  decodeWebBlobDescriptor,
  decodeWebContractBootstrap,
  decodeWebContractExample,
  decodeWebImportListPage,
} from "../../../clients/web/src/generated/web-contract.mjs";

const fixtureUrl = new URL("./fixtures/example.json", import.meta.url);

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
        contract: { name: "signalbox.web-http", version: "1" },
        capabilities: {
          bounded_json: true,
          bounded_session_timeline: true,
          same_origin_json_mutations: true,
          ndjson_streaming: true,
          immutable_blob_content: true,
          blob_derivations: true,
          image_derivatives: true,
          import_discovery: true,
          imported_continuations: true,
        },
        limits: {
          max_json_body_bytes: 65536,
          max_ndjson_item_bytes: 65536,
          max_timeline_window_items: 256,
          max_timeline_window_bytes: 65536,
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
