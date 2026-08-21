import { Download, FileQuestion, Image as ImageIcon, Maximize2 } from 'lucide-react'
import { useState } from 'react'
import type { WebBlobDescriptor } from '../../generated/web-contract.mjs'
import { artifactScenario } from './artifactScenario'
import './artifacts.css'

type WebBlobAvailableView = WebBlobDescriptor['available_views'][number]
type WebBlobViewKind = WebBlobAvailableView['kind']

const IMAGE_VIEW_PRIORITY: ReadonlyArray<WebBlobViewKind> = ['preview', 'thumbnail']

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

function ArtifactRenderer({ descriptor }: { descriptor: WebBlobDescriptor }) {
  const [originalRequested, setOriginalRequested] = useState(false)
  const automatic = selectImageView(descriptor)
  const original = viewByKind(descriptor, 'browser_native')
  const download = viewByKind(descriptor, 'download')
  const rendered = originalRequested && original ? original : automatic
  const derivation = rendered?.derivations[0]

  return (
    <article className="artifact-row" aria-label={`Artifact ${displayName(descriptor)}`}>
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
          {original && !originalRequested && (
            <button type="button" onClick={() => setOriginalRequested(true)}>
              <Maximize2 aria-hidden="true" /> Load original
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
          <ArtifactRenderer key={descriptor.digest} descriptor={descriptor} />
        ))}
      </div>
    </section>
  )
}
