import { decodeWebBlobDescriptor, type WebBlobDescriptor } from '../../generated/web-contract.mjs'
import {
  type ArtifactItem,
  boundArtifactText,
  type DerivativeArtifact,
  type DocumentArtifact,
  type MediaPlaceholderArtifact,
} from './artifactTypes'

const sourceDigest = 'sha256:3729b2319da081a0710ba27da7af330c1236325cf8ed0a619cf132375bb0fc1e'
const previewDigest = 'sha256:071d25f582ba9e6a8725e198dab884d70a3d7ce3ea84a74c66e65a1443c41a8e'
const documentDigest = `sha256:${'6f'.repeat(32)}`
const audioDigest = `sha256:${'7a'.repeat(32)}`
const videoDigest = `sha256:${'8c'.repeat(32)}`
const thumbnailDigest = 'sha256:e3f49e726a8b33752609b0f159cac0e185d6f02f6f72e872652e5df849ee5490'
const jpegDigest = 'sha256:11ce39dce155c991152fad639d7ba25efab3f14e9eb921f20d1dbde5b67cb29e'
type BlobView = WebBlobDescriptor['available_views'][number]

// Hard inline-original byte ceiling. A qualifying preview/thumbnail derivation additionally proves
// that the exact source digest passed the owning service's bounded image decoder (16,384 px per
// axis, 67,108,864 total pixels, and 320 MiB decoder allocation). Until the descriptor carries an
// aggregate animation-decode bound, only inherently single-frame JPEG originals can use that proof;
// animation-capable formats and originals without every proof remain ordinary-download only. The
// declared media type is caller-supplied, so admission is additionally verified against the actual
// fetched bytes at load time (fetchVerifiedSingleFrameJpeg below).
export const INLINE_ORIGINAL_MAX_BYTES = 16n * 1024n * 1024n
const BOUNDED_IMAGE_TRANSFORMATIONS = new Set(['image.preview', 'image.thumbnail'])
const SINGLE_FRAME_ORIGINAL_MEDIA_TYPES = new Set(['image/jpeg'])

const expectedTransformation = (kind: 'preview' | 'thumbnail') => ({
  name: kind === 'preview' ? 'image.preview' : 'image.thumbnail',
  parameters:
    kind === 'preview'
      ? '{"edge_px":1600,"format":"image/png"}'
      : '{"edge_px":256,"format":"image/png"}',
})

const viewOutputDigest = (view: BlobView): string | undefined =>
  /^\/api\/blobs\/(sha256:[0-9a-f]{64})\/content\//u.exec(view.content_url)?.[1]

// The owning service names a browser_native view from the caller-declared media type without
// inspecting stored bytes, and browsers sniff <img> bytes regardless of Content-Type, so a blob
// declared image/jpeg could actually be an animated GIF, WebP, or APNG. Until the descriptor
// carries a server-detected-format proof, the client fetches an admitted original and verifies the
// JPEG start-of-image signature before handing any bytes to the browser's renderer.
const JPEG_START_OF_IMAGE_SIGNATURE: readonly number[] = [0xff, 0xd8, 0xff]

export const isSingleFrameJpegBytes = (bytes: Uint8Array): boolean =>
  bytes.length >= JPEG_START_OF_IMAGE_SIGNATURE.length &&
  JPEG_START_OF_IMAGE_SIGNATURE.every((expected, index) => bytes[index] === expected)

export const fetchVerifiedSingleFrameJpeg = async (
  view: BlobView,
  fetchImplementation: typeof fetch,
  signal?: AbortSignal,
): Promise<Blob> => {
  const advertisedLength = BigInt(view.byte_length)
  if (advertisedLength > INLINE_ORIGINAL_MAX_BYTES) {
    throw new Error('original exceeds the inline admission ceiling')
  }
  const response = await fetchImplementation(view.content_url, { signal })
  if (!response.ok) {
    throw new Error(`original request failed with status ${String(response.status)}`)
  }
  if (response.body === null) {
    throw new Error('original response exposed no readable body')
  }
  // Consume the body through a bounded reader: a faulty response longer than the advertised
  // length is aborted mid-stream instead of being materialized before any size check runs.
  const expectedLength = Number(advertisedLength)
  const reader = response.body.getReader()
  const chunks: Uint8Array[] = []
  let receivedLength = 0
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    if (value === undefined) continue
    receivedLength += value.byteLength
    if (receivedLength > expectedLength) {
      await reader.cancel()
      throw new Error('original bytes exceed the advertised byte length')
    }
    chunks.push(value)
  }
  if (receivedLength !== expectedLength) {
    throw new Error('original bytes do not match the advertised byte length')
  }
  const bytes = new Uint8Array(receivedLength)
  let offset = 0
  for (const chunk of chunks) {
    bytes.set(chunk, offset)
    offset += chunk.byteLength
  }
  if (!isSingleFrameJpegBytes(bytes)) {
    throw new Error('original bytes are not a single-frame JPEG stream')
  }
  return new Blob([bytes], { type: view.media_type })
}

// One derivation record has to carry both proofs at once. The generated decoder checks them
// separately — the exact deterministic transformation for the advertised view kind against
// `derivations[0]`, the descriptor-input-to-content-output digest correlation against *any*
// derivation of the view — so a schema-valid descriptor can satisfy each with a different record.
// Admitting that union would let an arbitrary transformation's output render as the view and be
// reported as its proven provenance, so callers admit only a record proving the whole pair.
export const provesViewDerivation = (
  descriptor: WebBlobDescriptor,
  view: BlobView,
  derivation: BlobView['derivations'][number],
): boolean => {
  if (view.kind !== 'preview' && view.kind !== 'thumbnail') return false
  const expected = expectedTransformation(view.kind)
  const outputDigest = viewOutputDigest(view)
  return (
    BOUNDED_IMAGE_TRANSFORMATIONS.has(derivation.transformation_name) &&
    derivation.transformation_name === expected.name &&
    derivation.transformation_version === 1 &&
    derivation.parameters_json === expected.parameters &&
    derivation.producer.class === 'deterministic' &&
    derivation.input_digests.includes(descriptor.digest) &&
    outputDigest !== undefined &&
    derivation.output_digests.includes(outputDigest)
  )
}

export const selectProvenViewDerivation = (
  descriptor: WebBlobDescriptor,
  view: BlobView,
): BlobView['derivations'][number] | undefined =>
  view.derivations.find((derivation) => provesViewDerivation(descriptor, view, derivation))

export const selectBoundedOriginalView = (descriptor: WebBlobDescriptor): BlobView | undefined => {
  const original = descriptor.available_views.find((view) => view.kind === 'browser_native')
  if (
    original === undefined ||
    BigInt(original.byte_length) > INLINE_ORIGINAL_MAX_BYTES ||
    !SINGLE_FRAME_ORIGINAL_MEDIA_TYPES.has(original.media_type.toLowerCase())
  ) {
    return undefined
  }

  const boundedDecodeProven = descriptor.available_views.some(
    (view) => selectProvenViewDerivation(descriptor, view) !== undefined,
  )

  return boundedDecodeProven ? original : undefined
}

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

export const imageThumbnailView: BlobView = {
  kind: 'thumbnail',
  media_type: 'image/png',
  byte_length: '93',
  content_url: `/api/blobs/${thumbnailDigest}/content/image-png`,
  derivations: [
    {
      derivation_id: '0198f321-2300-7000-8000-000000000002',
      input_digests: [sourceDigest],
      transformation_name: 'image.thumbnail',
      transformation_version: 1,
      parameters_json: '{"edge_px":256,"format":"image/png"}',
      producer: {
        class: 'deterministic',
        implementation_digest: `sha256:${'4d'.repeat(32)}`,
        cache_key: 'sha256:62f6a23fafa777415ccee97b62957f0b08a0c1209545781e481b734e2aab9937',
      },
      output_digests: [thumbnailDigest],
    },
  ],
}

export const imageDescriptor = decodeWebBlobDescriptor({
  digest: sourceDigest,
  byte_length: '33749',
  declared_media_type: 'image/png',
  display_filename: ['orbital-map.png'],
  available_views: [imageDownloadView, imageOriginalView, imagePreviewView, imageThumbnailView],
})

export const jpegDownloadView: BlobView = {
  kind: 'download',
  media_type: 'image/jpeg',
  byte_length: '761',
  content_url: `/api/blobs/${jpegDigest}/download?media_type=image%2Fjpeg&display_filename=bounded-photo.jpg`,
  derivations: [],
}

export const jpegOriginalView: BlobView = {
  kind: 'browser_native',
  media_type: 'image/jpeg',
  byte_length: '761',
  content_url: `/api/blobs/${jpegDigest}/content/image-jpeg`,
  derivations: [],
}

export const jpegPreviewView: BlobView = {
  kind: 'preview',
  media_type: 'image/png',
  byte_length: '93',
  content_url: `/api/blobs/${thumbnailDigest}/content/image-png`,
  derivations: [
    {
      derivation_id: '0198f321-2300-7000-8000-000000000003',
      input_digests: [jpegDigest],
      transformation_name: 'image.preview',
      transformation_version: 1,
      parameters_json: '{"edge_px":1600,"format":"image/png"}',
      producer: {
        class: 'deterministic',
        implementation_digest: `sha256:${'4d'.repeat(32)}`,
        cache_key: 'sha256:6923cfb7e3a32e2c07915af6e91a8905e5e684abba9fed1d97750789aa5517cb',
      },
      output_digests: [thumbnailDigest],
    },
  ],
}

export const jpegDescriptor = decodeWebBlobDescriptor({
  digest: jpegDigest,
  byte_length: '761',
  declared_media_type: 'image/jpeg',
  display_filename: ['bounded-photo.jpg'],
  available_views: [jpegDownloadView, jpegOriginalView, jpegPreviewView],
})

const documentDescriptor = decodeWebBlobDescriptor({
  digest: documentDigest,
  byte_length: '1843200',
  declared_media_type: 'application/pdf',
  display_filename: ['architecture.pdf'],
  available_views: [
    {
      kind: 'download',
      media_type: 'application/pdf',
      byte_length: '1843200',
      content_url: `/api/blobs/${documentDigest}/download?media_type=application%2Fpdf&display_filename=architecture.pdf`,
      derivations: [],
    },
  ],
})

const audioDescriptor = decodeWebBlobDescriptor({
  digest: audioDigest,
  byte_length: '4194304',
  declared_media_type: 'audio/ogg',
  display_filename: ['operator-note.ogg'],
  available_views: [
    {
      kind: 'download',
      media_type: 'audio/ogg',
      byte_length: '4194304',
      content_url: `/api/blobs/${audioDigest}/download?media_type=audio%2Fogg&display_filename=operator-note.ogg`,
      derivations: [],
    },
  ],
})

const videoDescriptor = decodeWebBlobDescriptor({
  digest: videoDigest,
  byte_length: '73400320',
  declared_media_type: 'video/mp4',
  display_filename: ['runner-capture.mp4'],
  available_views: [
    {
      kind: 'download',
      media_type: 'video/mp4',
      byte_length: '73400320',
      content_url: `/api/blobs/${videoDigest}/download?media_type=video%2Fmp4&display_filename=runner-capture.mp4`,
      derivations: [],
    },
  ],
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
    id: 'bounded-photo',
    kind: 'image',
    displayName: 'bounded-photo.jpg',
    source: { kind: 'signalbox_blob', descriptor: jpegDescriptor },
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

export const documentAttachment: DocumentArtifact = {
  id: 'document-attachment',
  kind: 'document',
  displayName: 'architecture.pdf',
  documentKind: 'pdf',
  source: { kind: 'signalbox_blob', descriptor: documentDescriptor },
}

export const derivativeAttachment: DerivativeArtifact = {
  id: 'derived-attachment',
  kind: 'derivative',
  displayName: 'orbital-map.preview.png',
  presentation: 'image',
  viewKind: 'preview',
  source: { kind: 'signalbox_blob', descriptor: imageDescriptor },
}

export const audioAttachment: MediaPlaceholderArtifact = {
  id: 'audio-attachment',
  kind: 'media_placeholder',
  displayName: 'operator-note.ogg',
  mediaKind: 'audio',
  source: { kind: 'signalbox_blob', descriptor: audioDescriptor },
}

export const videoAttachment: MediaPlaceholderArtifact = {
  id: 'video-attachment',
  kind: 'media_placeholder',
  displayName: 'runner-capture.mp4',
  mediaKind: 'video',
  source: { kind: 'signalbox_blob', descriptor: videoDescriptor },
}

export const attachmentScenario: ReadonlyArray<ArtifactItem> = [
  documentAttachment,
  derivativeAttachment,
  audioAttachment,
  videoAttachment,
]

export const imageArtifact = imageDescriptor
export const artifactPreviewIds = artifactScenario
  .filter(
    (artifact) =>
      (artifact.kind === 'text' || artifact.kind === 'code') &&
      boundArtifactText(artifact.content, artifact.characterCount, 'preview').omittedCharacters > 0,
  )
  .map((artifact) => artifact.id)
export const artifactOriginalIds = artifactScenario.flatMap((artifact) =>
  artifact.kind === 'image' &&
  artifact.source.kind === 'signalbox_blob' &&
  selectBoundedOriginalView(artifact.source.descriptor) !== undefined
    ? [artifact.id]
    : [],
)
