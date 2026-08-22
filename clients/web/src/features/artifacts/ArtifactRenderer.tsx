import { Download, FileQuestion, Image as ImageIcon, Maximize2 } from 'lucide-react'
import { useCallback, useEffect, useState } from 'react'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { artifactScenario } from './artifactScenario'
import './artifacts.css'

type WebBlobAvailableView = WebBlobDescriptor['available_views'][number]
type WebBlobViewKind = WebBlobAvailableView['kind']

const IMAGE_VIEW_PRIORITY: ReadonlyArray<WebBlobViewKind> = ['preview', 'thumbnail']

// Hard client ceiling: larger originals remain download-only so one image cannot force the
// browser to fetch and decode deployment-sized blob content.
export const MAX_INLINE_ORIGINAL_BYTES = 16 * 1024 * 1024
export const MAX_INLINE_ORIGINAL_PIXELS = 40_000_000
const MAX_IMAGE_HEADER_BYTES = 64 * 1024

export const isInlineOriginalByteLengthAdmitted = (byteLength: string): boolean =>
  BigInt(byteLength) <= BigInt(MAX_INLINE_ORIGINAL_BYTES)

export const selectImageView = (descriptor: WebBlobDescriptor): WebBlobAvailableView | undefined =>
  IMAGE_VIEW_PRIORITY.map((kind) =>
    descriptor.available_views.find((view) => view.kind === kind),
  ).find((view) => view !== undefined)

export const imageViewLabel = (kind: WebBlobViewKind): string =>
  ({
    browser_native: 'Original',
    preview: 'Preview',
    thumbnail: 'Thumbnail',
    download: 'Download',
  })[kind]

const viewByKind = (
  descriptor: WebBlobDescriptor,
  kind: WebBlobViewKind,
): WebBlobAvailableView | undefined => descriptor.available_views.find((view) => view.kind === kind)

const displayName = (descriptor: WebBlobDescriptor): string =>
  descriptor.display_filename[0] ?? descriptor.digest

const readImageHeader = async (url: string): Promise<Uint8Array> => {
  const response = await fetch(url, {
    headers: { Range: `bytes=0-${MAX_IMAGE_HEADER_BYTES - 1}` },
  })
  if (!response.ok) throw new Error(`image header request failed with status ${response.status}`)
  const reader = response.body?.getReader()
  if (!reader) throw new Error('image header response had no body')
  const chunks: Uint8Array[] = []
  let received = 0
  while (true) {
    const result = await reader.read()
    if (result.done) break
    received += result.value.byteLength
    if (received > MAX_IMAGE_HEADER_BYTES) {
      await reader.cancel()
      throw new Error('image dimensions were not available within the bounded header')
    }
    chunks.push(result.value)
  }
  const bytes = new Uint8Array(received)
  let offset = 0
  for (const chunk of chunks) {
    bytes.set(chunk, offset)
    offset += chunk.byteLength
  }
  return bytes
}

export const readImageDimensions = (
  bytes: Uint8Array,
): { width: number; height: number } | null => {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
  if (bytes.length >= 24 && view.getUint32(0) === 0x89504e47 && view.getUint32(4) === 0x0d0a1a0a) {
    return { width: view.getUint32(16), height: view.getUint32(20) }
  }
  if (bytes.length >= 10 && new TextDecoder().decode(bytes.subarray(0, 6)).startsWith('GIF8')) {
    return { width: view.getUint16(6, true), height: view.getUint16(8, true) }
  }
  if (
    bytes.length >= 30 &&
    new TextDecoder().decode(bytes.subarray(0, 4)) === 'RIFF' &&
    new TextDecoder().decode(bytes.subarray(8, 12)) === 'WEBP'
  ) {
    const kind = new TextDecoder().decode(bytes.subarray(12, 16))
    if (kind === 'VP8X') {
      return {
        width: 1 + view.getUint8(24) + (view.getUint8(25) << 8) + (view.getUint8(26) << 16),
        height: 1 + view.getUint8(27) + (view.getUint8(28) << 8) + (view.getUint8(29) << 16),
      }
    }
  }
  if (bytes.length >= 4 && bytes[0] === 0xff && bytes[1] === 0xd8) {
    let offset = 2
    while (offset + 9 < bytes.length) {
      if (bytes[offset] !== 0xff) {
        offset += 1
        continue
      }
      const marker = view.getUint8(offset + 1)
      const length = view.getUint16(offset + 2)
      if (length < 2 || offset + length + 2 > bytes.length) return null
      const startOfFrame =
        (marker >= 0xc0 && marker <= 0xc3) ||
        (marker >= 0xc5 && marker <= 0xc7) ||
        (marker >= 0xc9 && marker <= 0xcb) ||
        (marker >= 0xcd && marker <= 0xcf)
      if (startOfFrame) {
        return { width: view.getUint16(offset + 7), height: view.getUint16(offset + 5) }
      }
      offset += length + 2
    }
  }
  return null
}

export function ArtifactRenderer({
  compact = false,
  descriptor,
  originalRequested = false,
  onOriginalRequested,
}: {
  compact?: boolean
  descriptor: WebBlobDescriptor
  originalRequested?: boolean
  onOriginalRequested?: () => void
}) {
  const automatic = selectImageView(descriptor)
  const original = viewByKind(descriptor, 'browser_native')
  const originalWithinByteLimit = original
    ? isInlineOriginalByteLengthAdmitted(original.byte_length)
    : false
  const [originalStatus, setOriginalStatus] = useState<
    'idle' | 'checking' | 'admitted' | 'rejected'
  >('idle')
  const originalAdmitted = originalStatus === 'admitted'
  const download = viewByKind(descriptor, 'download')
  const rendered = originalRequested && originalAdmitted ? original : automatic
  const derivation = rendered?.derivations[0]

  const admitOriginal = useCallback(() => {
    if (!original || !originalWithinByteLimit || originalStatus === 'checking') return
    setOriginalStatus('checking')
    void readImageHeader(original.content_url)
      .then((bytes) => {
        const dimensions = readImageDimensions(bytes)
        if (
          !dimensions ||
          dimensions.width <= 0 ||
          dimensions.height <= 0 ||
          dimensions.width * dimensions.height > MAX_INLINE_ORIGINAL_PIXELS
        ) {
          setOriginalStatus('rejected')
          return
        }
        setOriginalStatus('admitted')
        onOriginalRequested?.()
      })
      .catch(() => setOriginalStatus('rejected'))
  }, [onOriginalRequested, original, originalStatus, originalWithinByteLimit])

  useEffect(() => {
    if (originalRequested && originalWithinByteLimit && originalStatus === 'idle') {
      admitOriginal()
    }
  }, [admitOriginal, originalRequested, originalStatus, originalWithinByteLimit])

  return (
    <article
      className={compact ? 'artifact-row artifact-row-compact' : 'artifact-row'}
      aria-label={`Artifact ${displayName(descriptor)}`}
    >
      <div className="artifact-visual">
        {rendered ? (
          <img
            src={rendered.content_url}
            alt={`${imageViewLabel(rendered.kind)} of ${displayName(descriptor)}`}
            loading="lazy"
          />
        ) : (
          <FileQuestion aria-label="No compatible inline renderer" />
        )}
      </div>
      <div className="artifact-detail">
        <header>
          {rendered ? <ImageIcon aria-hidden="true" /> : <FileQuestion aria-hidden="true" />}
          <div>
            <strong>{displayName(descriptor)}</strong>
            <small>{descriptor.byte_length} bytes · immutable original</small>
          </div>
        </header>
        <dl>
          <div>
            <dt>Renderer</dt>
            <dd>{rendered?.kind ?? 'metadata fallback'}</dd>
          </div>
          <div>
            <dt>Declared type</dt>
            <dd>{descriptor.declared_media_type}</dd>
          </div>
          <div>
            <dt>Provenance</dt>
            <dd>{derivation?.transformation_name ?? 'original bytes'}</dd>
          </div>
        </dl>
        <div className="artifact-actions">
          {original && (
            <button
              type="button"
              aria-pressed={originalRequested && originalAdmitted}
              disabled={
                !originalWithinByteLimit ||
                originalStatus === 'checking' ||
                originalStatus === 'rejected'
              }
              onClick={() => {
                if (originalAdmitted) onOriginalRequested?.()
                else admitOriginal()
              }}
            >
              <Maximize2 aria-hidden="true" />
              {originalRequested && originalAdmitted
                ? 'Original loaded'
                : originalStatus === 'checking'
                  ? 'Checking original dimensions…'
                  : originalStatus === 'rejected'
                    ? 'Original exceeds safe pixel limit; download only'
                    : originalWithinByteLimit
                      ? 'Load original'
                      : 'Original exceeds 16 MiB inline limit'}
            </button>
          )}
          {download && (
            <a href={download.content_url} download={displayName(descriptor)}>
              <Download aria-hidden="true" /> Download
            </a>
          )}
        </div>
      </div>
    </article>
  )
}

function StatefulArtifactRenderer({ descriptor }: { descriptor: WebBlobDescriptor }) {
  const [originalRequested, setOriginalRequested] = useState(false)
  return (
    <ArtifactRenderer
      descriptor={descriptor}
      originalRequested={originalRequested}
      onOriginalRequested={() => setOriginalRequested(true)}
    />
  )
}

export function ArtifactWorkbench() {
  return (
    <section
      className="artifact-panel"
      aria-label="Blob evidence"
      data-command-focus-target
      tabIndex={-1}
    >
      <header className="section-header">
        <div>
          <span className="eyebrow">Capability projection</span>
          <h1 id="artifact-heading">Blob evidence</h1>
        </div>
        <span className="window-count">2 descriptors · 0 original bytes prefetched</span>
      </header>
      <div className="artifact-list">
        {artifactScenario.map((descriptor) => (
          <StatefulArtifactRenderer key={descriptor.digest} descriptor={descriptor} />
        ))}
      </div>
    </section>
  )
}
