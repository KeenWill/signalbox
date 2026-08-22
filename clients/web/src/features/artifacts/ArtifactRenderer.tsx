import { Download, FileQuestion, Image as ImageIcon, Maximize2 } from 'lucide-react'
import { useState } from 'react'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { artifactScenario } from './artifactScenario'
import './artifacts.css'

type WebBlobAvailableView = WebBlobDescriptor['available_views'][number]
type WebBlobViewKind = WebBlobAvailableView['kind']

const IMAGE_VIEW_PRIORITY: ReadonlyArray<WebBlobViewKind> = ['preview', 'thumbnail']

// Hard client ceiling: larger originals remain download-only so one image cannot force the
// browser to fetch and decode deployment-sized blob content.
export const MAX_INLINE_ORIGINAL_BYTES = 16 * 1024 * 1024

export const isInlineOriginalByteLengthAdmitted = (byteLength: string): boolean =>
  BigInt(byteLength) <= BigInt(MAX_INLINE_ORIGINAL_BYTES)

export const selectImageView = (descriptor: WebBlobDescriptor): WebBlobAvailableView | undefined =>
  IMAGE_VIEW_PRIORITY.map((kind) =>
    descriptor.available_views.find((view) => view.kind === kind),
  ).find((view) => view !== undefined)

const viewByKind = (
  descriptor: WebBlobDescriptor,
  kind: WebBlobViewKind,
): WebBlobAvailableView | undefined => descriptor.available_views.find((view) => view.kind === kind)

const displayName = (descriptor: WebBlobDescriptor): string =>
  descriptor.display_filename[0] ?? descriptor.digest

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
  // The descriptor does not expose decoded dimensions, so the client cannot prove a
  // pixel ceiling before assigning the original URL. Originals remain download-only.
  const originalAdmitted = false
  const download = viewByKind(descriptor, 'download')
  const rendered = originalRequested && originalAdmitted ? original : automatic
  const derivation = rendered?.derivations[0]

  return (
    <article
      className={compact ? 'artifact-row artifact-row-compact' : 'artifact-row'}
      aria-label={`Artifact ${displayName(descriptor)}`}
    >
      <div className="artifact-visual">
        {rendered ? (
          <img
            src={rendered.content_url}
            alt={`${rendered.kind === 'browser_native' ? 'Original' : 'Preview'} of ${displayName(descriptor)}`}
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
              disabled={!originalAdmitted}
              onClick={onOriginalRequested}
            >
              <Maximize2 aria-hidden="true" />
              {originalAdmitted
                ? originalRequested
                  ? 'Original loaded'
                  : 'Load original'
                : originalWithinByteLimit
                  ? 'Original dimensions unavailable; download only'
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
    <section className="artifact-panel" aria-labelledby="artifact-heading">
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
