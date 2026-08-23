import { decodeWebBlobDescriptor, type WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { type ArtifactItem, boundArtifactText } from './artifactTypes'

const sourceDigest = 'sha256:3729b2319da081a0710ba27da7af330c1236325cf8ed0a619cf132375bb0fc1e'
const previewDigest = 'sha256:071d25f582ba9e6a8725e198dab884d70a3d7ce3ea84a74c66e65a1443c41a8e'
type BlobView = WebBlobDescriptor['available_views'][number]

export const imageDownloadView: BlobView = {
  kind: 'download',
  media_type: 'image/png',
  byte_length: '33749',
  content_url: `/api/blobs/${sourceDigest}/download?media_type=image%2Fpng&display_filename=orbital-map.png`,
  derivations: [],
}

export const imageOriginalView: BlobView = {
  kind: 'browser_native',
  media_type: 'image/png',
  byte_length: '33749',
  content_url: `/api/blobs/${sourceDigest}/content/image-png`,
  derivations: [],
}

export const imagePreviewView: BlobView = {
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
}

export const imageDescriptor = decodeWebBlobDescriptor({
  digest: sourceDigest,
  byte_length: '33749',
  declared_media_type: 'image/png',
  display_filename: ['orbital-map.png'],
  available_views: [imageDownloadView, imageOriginalView, imagePreviewView],
})

const fallbackDigest = `sha256:${'3c'.repeat(32)}`
export const fallbackDownloadView: BlobView = {
  kind: 'download',
  media_type: 'application/octet-stream',
  byte_length: '4096',
  content_url: `/api/blobs/${fallbackDigest}/download?media_type=application%2Foctet-stream&display_filename=trace.bin`,
  derivations: [],
}
export const fallbackDescriptor = decodeWebBlobDescriptor({
  digest: fallbackDigest,
  byte_length: '4096',
  declared_media_type: 'application/octet-stream',
  display_filename: ['trace.bin'],
  available_views: [fallbackDownloadView],
})

const generatedText = Array.from(
  { length: 180 },
  (_, index) => `line ${String(index + 1).padStart(3, '0')} — bounded incident chronology`,
).join('\n')

const generatedCode = Array.from(
  { length: 240 },
  (_, index) => `const sample_${index + 1} = inspectArtifact(${index + 1})`,
).join('\n')

export const artifactScenario: ReadonlyArray<ArtifactItem> = [
  {
    id: 'incident-notes',
    kind: 'text',
    displayName: 'incident-notes.txt',
    content: generatedText,
    characterCount: Array.from(generatedText).length,
  },
  {
    id: 'renderer-source',
    kind: 'code',
    displayName: 'renderer.ts',
    language: 'TypeScript',
    content: generatedCode,
    characterCount: Array.from(generatedCode).length,
  },
  {
    id: 'orbital-map',
    kind: 'image',
    displayName: 'orbital-map.png',
    source: { kind: 'signalbox_blob', descriptor: imageDescriptor },
  },
  {
    id: 'descriptor-fallback',
    kind: 'blob',
    displayName: 'trace.bin',
    descriptor: fallbackDescriptor,
  },
  {
    id: 'remote-diagram',
    kind: 'image',
    displayName: 'remote-status-diagram.png',
    source: {
      kind: 'remote',
      url: 'https://media.example.test/remote-status-diagram.png',
      alt: 'Remote status diagram',
    },
  },
  {
    id: 'restricted-capture',
    kind: 'blocked',
    displayName: 'restricted.capture',
    attemptedKind: 'unknown binary',
    reason: 'The current capability projection does not authorize a content view.',
  },
]

export const imageArtifact = imageDescriptor
export const artifactPreviewIds = artifactScenario
  .filter(
    (artifact) =>
      (artifact.kind === 'text' || artifact.kind === 'code') &&
      boundArtifactText(artifact.content, artifact.characterCount, 'preview').omittedCharacters > 0,
  )
  .map((artifact) => artifact.id)
// Browser-native descriptors do not carry an enforceable decoded-dimension ceiling, so no
// original is currently eligible for inline rendering. Originals remain available for download.
export const artifactOriginalIds: ReadonlyArray<string> = []
