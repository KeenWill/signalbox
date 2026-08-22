import { decodeWebBlobDescriptor, type WebBlobDescriptor } from '../../generated/web-contract.mjs'

const sourceDigest = 'sha256:3729b2319da081a0710ba27da7af330c1236325cf8ed0a619cf132375bb0fc1e'
const previewDigest = 'sha256:071d25f582ba9e6a8725e198dab884d70a3d7ce3ea84a74c66e65a1443c41a8e'
const binaryDigest = `sha256:${'3c'.repeat(32)}`

export const imageArtifact = decodeWebBlobDescriptor({
  digest: sourceDigest,
  byte_length: '33749',
  declared_media_type: 'image/png',
  display_filename: ['orbital-map.png'],
  available_views: [
    {
      kind: 'download',
      media_type: 'image/png',
      byte_length: '33749',
      content_url: `/api/blobs/${sourceDigest}/download?media_type=image%2Fpng&display_filename=orbital-map.png`,
      derivations: [],
    },
    {
      kind: 'browser_native',
      media_type: 'image/png',
      byte_length: '33749',
      content_url: `/api/blobs/${sourceDigest}/content/image-png`,
      derivations: [],
    },
    {
      kind: 'preview',
      media_type: 'image/png',
      byte_length: '215370',
      content_url: `/api/blobs/${previewDigest}/content/image-png`,
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
            cache_key: 'sha256:07257dcebadabd8928bfae61ebcf7c45ead3d35cf94cfdacf572f40695668816',
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
      content_url: `/api/blobs/${binaryDigest}/download?media_type=application%2Foctet-stream&display_filename=telemetry.capture`,
      derivations: [],
    },
  ],
})

export const artifactScenario: ReadonlyArray<WebBlobDescriptor> = [imageArtifact, binaryArtifact]
