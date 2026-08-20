import { decodeWebBlobDescriptor, type WebBlobDescriptor } from '../../generated/web-contract.mjs'

const sourceDigest = `sha256:${'1a'.repeat(32)}`
const previewDigest = `sha256:${'2b'.repeat(32)}`
const binaryDigest = `sha256:${'3c'.repeat(32)}`

export const imageArtifact = decodeWebBlobDescriptor({
  digest: sourceDigest,
  byte_length: '94371840',
  declared_media_type: 'image/png',
  display_filename: ['orbital-map.png'],
  available_views: [
    {
      kind: 'download',
      media_type: 'image/png',
      byte_length: '94371840',
      content_url: `/api/blobs/${sourceDigest}/download`,
      derivations: [],
    },
    {
      kind: 'browser_native',
      media_type: 'image/png',
      byte_length: '94371840',
      content_url: `/api/blobs/${sourceDigest}/content/image.svg`,
      derivations: [],
    },
    {
      kind: 'preview',
      media_type: 'image/svg+xml',
      byte_length: '842',
      content_url: `/api/blobs/${previewDigest}/content/image.svg`,
      derivations: [
        {
          derivation_id: '0198f321-2300-7000-8000-000000000001',
          input_digests: [sourceDigest],
          transformation_name: 'image.preview',
          transformation_version: 1,
          parameters_json: '{"edge_px":1600,"format":"image/png"}',
          producer: {
            class: 'deterministic',
            implementation_digest: `sha256:${'4d'.repeat(32)}`,
            cache_key: `sha256:${'5e'.repeat(32)}`,
          },
          output_digests: [previewDigest],
        },
      ],
    },
  ],
})

export const binaryArtifact = decodeWebBlobDescriptor({
  digest: binaryDigest,
  byte_length: '734003200',
  declared_media_type: 'application/octet-stream',
  display_filename: ['telemetry.capture'],
  available_views: [
    {
      kind: 'download',
      media_type: 'application/octet-stream',
      byte_length: '734003200',
      content_url: `/api/blobs/${binaryDigest}/download`,
      derivations: [],
    },
  ],
})

export const artifactScenario: ReadonlyArray<WebBlobDescriptor> = [imageArtifact, binaryArtifact]
