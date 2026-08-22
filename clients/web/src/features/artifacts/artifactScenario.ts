import { decodeWebBlobDescriptor, type WebBlobDescriptor } from '../../generated/web-contract.mjs'
import type {
  ArtifactItem,
  DerivativeArtifact,
  DocumentArtifact,
  MediaPlaceholderArtifact,
} from './artifactTypes'

const sourceDigest = `sha256:${'1a'.repeat(32)}`
const previewDigest = `sha256:${'2b'.repeat(32)}`
const documentDigest = `sha256:${'6f'.repeat(32)}`
const audioDigest = `sha256:${'7a'.repeat(32)}`
const videoDigest = `sha256:${'8c'.repeat(32)}`
type BlobView = WebBlobDescriptor['available_views'][number]

export const imageDownloadView: BlobView = {
  kind: 'download',
  media_type: 'image/png',
  byte_length: '62914560',
  content_url: `/api/blobs/${sourceDigest}/download?media_type=image%2Fpng&display_filename=orbital-map.png`,
  derivations: [],
}

export const imageOriginalView: BlobView = {
  kind: 'browser_native',
  media_type: 'image/png',
  byte_length: '62914560',
  content_url: `/api/blobs/${sourceDigest}/content/image-png`,
  derivations: [],
}

export const imagePreviewView: BlobView = {
  kind: 'preview',
  media_type: 'image/png',
  byte_length: '842',
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
        cache_key: `sha256:${'5e'.repeat(32)}`,
      },
      output_digests: [previewDigest],
    },
  ],
}

export const imageDescriptor = decodeWebBlobDescriptor({
  digest: sourceDigest,
  byte_length: '62914560',
  declared_media_type: 'image/png',
  display_filename: ['orbital-map.png'],
  available_views: [imageDownloadView, imageOriginalView, imagePreviewView],
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
  },
  {
    id: 'renderer-source',
    kind: 'code',
    displayName: 'renderer.ts',
    language: 'TypeScript',
    content: generatedCode,
  },
  {
    id: 'orbital-map',
    kind: 'image',
    displayName: 'orbital-map.png',
    source: { kind: 'signalbox_blob', descriptor: imageDescriptor },
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
    id: 'future-pdf',
    kind: 'committed_unimplemented',
    displayName: 'architecture.pdf',
    attemptedKind: 'document',
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
