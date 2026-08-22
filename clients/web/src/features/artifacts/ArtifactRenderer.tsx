import { Download, FileQuestion, Image as ImageIcon, Maximize2 } from 'lucide-react'
import { useCallback, useEffect, useRef, useState } from 'react'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { productTransport } from '../../product'
import { artifactScenario } from './artifactScenario'
import './artifacts.css'

type WebBlobAvailableView = WebBlobDescriptor['available_views'][number]
type WebBlobViewKind = WebBlobAvailableView['kind']

const IMAGE_VIEW_PRIORITY: ReadonlyArray<WebBlobViewKind> = ['preview', 'thumbnail']

// Hard client ceiling: larger originals remain download-only so one image cannot force the
// browser to fetch and decode deployment-sized blob content.
export const MAX_INLINE_ORIGINAL_BYTES = 16 * 1024 * 1024
export const MAX_INLINE_ORIGINAL_PIXELS = 40_000_000
const MAX_IMAGE_INSPECTION_BYTES = MAX_INLINE_ORIGINAL_BYTES

export const isInlineOriginalByteLengthAdmitted = (byteLength: string): boolean =>
  BigInt(byteLength) <= BigInt(MAX_INLINE_ORIGINAL_BYTES)

export const isInlineOriginalLengthAdmitted = (
  descriptorByteLength: string,
  originalByteLength: string,
): boolean =>
  descriptorByteLength === originalByteLength &&
  isInlineOriginalByteLengthAdmitted(originalByteLength)

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

const readAsciiTag = (bytes: Uint8Array, offset: number, length = 4): string =>
  String.fromCharCode(...bytes.subarray(offset, offset + length))

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
    bytes.length >= 25 &&
    readAsciiTag(bytes, 0) === 'RIFF' &&
    readAsciiTag(bytes, 8) === 'WEBP'
  ) {
    const kind = readAsciiTag(bytes, 12)
    if (kind === 'VP8X' && bytes.length >= 30) {
      return {
        width: 1 + view.getUint8(24) + (view.getUint8(25) << 8) + (view.getUint8(26) << 16),
        height: 1 + view.getUint8(27) + (view.getUint8(28) << 8) + (view.getUint8(29) << 16),
      }
    }
    if (
      kind === 'VP8 ' &&
      bytes.length >= 30 &&
      view.getUint8(23) === 0x9d &&
      view.getUint8(24) === 0x01 &&
      view.getUint8(25) === 0x2a
    ) {
      return { width: view.getUint16(26, true) & 0x3fff, height: view.getUint16(28, true) & 0x3fff }
    }
    if (kind === 'VP8L' && view.getUint8(20) === 0x2f) {
      return {
        width: 1 + view.getUint8(21) + ((view.getUint8(22) & 0x3f) << 8),
        height:
          1 +
          (view.getUint8(22) >> 6) +
          (view.getUint8(23) << 2) +
          ((view.getUint8(24) & 0x0f) << 10),
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
      while (offset < bytes.length && bytes[offset] === 0xff) offset += 1
      if (offset >= bytes.length) return null
      const marker = view.getUint8(offset)
      if (marker === 0x00) {
        offset += 1
        continue
      }
      if (
        marker === 0x01 ||
        marker === 0xd8 ||
        marker === 0xd9 ||
        (marker >= 0xd0 && marker <= 0xd7)
      ) {
        offset += 1
        continue
      }
      const markerOffset = offset - 1
      if (offset + 2 >= bytes.length) return null
      const length = view.getUint16(offset + 1)
      if (length < 2 || markerOffset + length + 2 > bytes.length) return null
      const startOfFrame =
        (marker >= 0xc0 && marker <= 0xc3) ||
        (marker >= 0xc5 && marker <= 0xc7) ||
        (marker >= 0xc9 && marker <= 0xcb) ||
        (marker >= 0xcd && marker <= 0xcf)
      if (startOfFrame) {
        return {
          width: view.getUint16(markerOffset + 7),
          height: view.getUint16(markerOffset + 5),
        }
      }
      offset = markerOffset + length + 2
    }
  }
  return null
}

export const isAnimationSafeImageHeader = (bytes: Uint8Array): boolean => {
  if (bytes.length < 12) return false
  if (readAsciiTag(bytes, 0) === 'GIF8') {
    if (bytes.length < 14) return false
    let offset = 13
    const packedFields = bytes[10] ?? 0
    if ((packedFields & 0x80) !== 0) offset += 3 * 2 ** ((packedFields & 0x07) + 1)
    let frames = 0
    const skipSubBlocks = () => {
      while (offset < bytes.length) {
        const length = bytes[offset] ?? 0
        offset += 1
        if (length === 0) return true
        offset += length
        if (offset > bytes.length) return false
      }
      return false
    }
    while (offset < bytes.length) {
      const marker = bytes[offset]
      offset += 1
      if (marker === 0x3b) return frames === 1
      if (marker === 0x21) {
        if (offset >= bytes.length) return false
        offset += 1
        if (!skipSubBlocks()) return false
        continue
      }
      if (marker !== 0x2c || offset + 9 > bytes.length) return false
      frames += 1
      if (frames > 1) return false
      const packed = bytes[offset + 8] ?? 0
      offset += 9
      if ((packed & 0x80) !== 0) offset += 3 * 2 ** ((packed & 0x07) + 1)
      if (offset >= bytes.length) return false
      offset += 1
      if (!skipSubBlocks()) return false
    }
    return false
  }
  if (readAsciiTag(bytes, 8) === 'WEBP') {
    const kind = readAsciiTag(bytes, 12)
    return (
      kind === 'VP8 ' || kind === 'VP8L' || (kind === 'VP8X' && ((bytes[20] ?? 0) & 0x02) === 0)
    )
  }
  if (bytes[0] === 0xff && bytes[1] === 0xd8) return true
  if (!(bytes[0] === 0x89 && readAsciiTag(bytes, 1, 3) === 'PNG')) return false
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
  let offset = 8
  while (offset + 12 <= bytes.length) {
    const length = view.getUint32(offset)
    const kind = readAsciiTag(bytes, offset + 4)
    if (kind === 'acTL') return false
    if (kind === 'IDAT') return true
    const end = offset + 12 + length
    if (end > bytes.length) return false
    offset = end
  }
  return false
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
  onOriginalRequested?: (digest: string) => void
}) {
  const automatic = selectImageView(descriptor)
  const original = viewByKind(descriptor, 'browser_native')
  const originalWithinByteLimit = original
    ? isInlineOriginalLengthAdmitted(descriptor.byte_length, original.byte_length)
    : false
  const [originalStatus, setOriginalStatus] = useState<
    'idle' | 'checking' | 'admitted' | 'rejected' | 'failed'
  >('idle')
  const probeController = useRef<AbortController | null>(null)
  const originalAdmitted = originalStatus === 'admitted'
  const download = viewByKind(descriptor, 'download')
  const rendered = originalRequested && originalAdmitted ? original : automatic
  const derivation = rendered?.derivations[0]

  const admitOriginal = useCallback(() => {
    if (!original || !originalWithinByteLimit || originalStatus === 'checking') return
    probeController.current?.abort()
    const controller = new AbortController()
    probeController.current = controller
    setOriginalStatus('checking')
    void productTransport
      .readBlobHeader(
        {
          contentUrl: original.content_url,
          digest: descriptor.digest,
          byteLength: descriptor.byte_length,
          maxBytes: MAX_IMAGE_INSPECTION_BYTES,
        },
        controller.signal,
      )
      .then((bytes) => {
        if (controller.signal.aborted) return
        const dimensions = readImageDimensions(bytes)
        if (
          !dimensions ||
          !isAnimationSafeImageHeader(bytes) ||
          dimensions.width <= 0 ||
          dimensions.height <= 0 ||
          dimensions.width * dimensions.height > MAX_INLINE_ORIGINAL_PIXELS
        ) {
          setOriginalStatus('rejected')
          return
        }
        setOriginalStatus('admitted')
        onOriginalRequested?.(descriptor.digest)
      })
      .catch(() => {
        if (!controller.signal.aborted) setOriginalStatus('failed')
      })
  }, [
    descriptor.byte_length,
    descriptor.digest,
    onOriginalRequested,
    original,
    originalStatus,
    originalWithinByteLimit,
  ])

  useEffect(() => {
    return () => probeController.current?.abort()
  }, [])

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
                if (originalAdmitted) onOriginalRequested?.(descriptor.digest)
                else admitOriginal()
              }}
            >
              <Maximize2 aria-hidden="true" />
              {originalRequested && originalAdmitted
                ? 'Original loaded'
                : originalStatus === 'checking'
                  ? 'Checking original dimensions…'
                  : originalStatus === 'rejected'
                    ? 'Original is not safe for inline rendering; download only'
                    : originalStatus === 'failed'
                      ? 'Retry original check'
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
