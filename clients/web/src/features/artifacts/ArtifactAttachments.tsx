import { FileQuestion, Paperclip, X } from 'lucide-react'
import { useMemo, useState } from 'react'
import { ArtifactRenderer } from './ArtifactRenderer'
import { attachmentScenario } from './artifactScenario'
import type { ArtifactItem } from './artifactTypes'
import { useRemoteMediaPreference } from './remoteMediaPreference'

export const MAX_VISIBLE_ATTACHMENTS = 12

export const boundAttachments = (
  items: ReadonlyArray<ArtifactItem>,
): { visible: ReadonlyArray<ArtifactItem>; omitted: number } => ({
  visible: items.slice(0, MAX_VISIBLE_ATTACHMENTS),
  omitted: Math.max(items.length - MAX_VISIBLE_ATTACHMENTS, 0),
})

const attachmentKind = (artifact: ArtifactItem): string => {
  if (artifact.kind === 'committed_unimplemented' || artifact.kind === 'blocked') {
    return artifact.attemptedKind
  }
  if (artifact.kind === 'media_placeholder') return `${artifact.mediaKind} placeholder`
  return artifact.kind
}

function AttachmentList({
  label,
  items,
  selectedId,
  onSelect,
  onRemove,
}: {
  label: string
  items: ReadonlyArray<ArtifactItem>
  selectedId: string | null
  onSelect: (artifact: ArtifactItem) => void
  onRemove?: (artifact: ArtifactItem) => void
}) {
  const bounded = boundAttachments(items)

  return (
    <section className="attachment-surface" aria-label={label}>
      <header>
        <div>
          <Paperclip aria-hidden="true" />
          <strong>{label}</strong>
        </div>
        <span>
          {bounded.visible.length} shown
          {bounded.omitted > 0 ? ` · ${bounded.omitted} omitted` : ''}
        </span>
      </header>
      {bounded.visible.length === 0 ? (
        <p className="attachment-empty">No attachments in this bounded client view.</p>
      ) : (
        <ul>
          {bounded.visible.map((artifact) => (
            <li key={artifact.id}>
              <button
                type="button"
                className="attachment-select"
                aria-pressed={selectedId === artifact.id}
                onClick={() => onSelect(artifact)}
              >
                <span className="attachment-name">{artifact.displayName}</span>
                <small>{attachmentKind(artifact)}</small>
              </button>
              {onRemove && (
                <button
                  type="button"
                  className="attachment-remove"
                  aria-label={`Remove ${artifact.displayName}`}
                  onClick={() => onRemove(artifact)}
                >
                  <X aria-hidden="true" />
                </button>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}

export function MissingAttachmentState({ placement }: { placement: 'composer' | 'transcript' }) {
  return (
    <section className="attachment-missing" aria-label={`${placement} attachments unavailable`}>
      <FileQuestion aria-hidden="true" />
      <div>
        <strong>
          {placement === 'composer' ? 'Composer attachments' : 'Transcript attachments'}
        </strong>
        <p>
          Committed · unavailable. The current browser contract exposes no attachment descriptors
          for this {placement}; no blob URL or media type was inferred.
        </p>
      </div>
    </section>
  )
}

export function AttachmentWorkbench() {
  const [remoteMedia] = useRemoteMediaPreference()
  const [composerItems, setComposerItems] =
    useState<ReadonlyArray<ArtifactItem>>(attachmentScenario)
  const [selectedId, setSelectedId] = useState(attachmentScenario[0]?.id ?? null)
  const allItems = useMemo(() => [...attachmentScenario, ...composerItems], [composerItems])
  const selected = allItems.find((artifact) => artifact.id === selectedId) ?? null

  return (
    <section className="attachment-workbench" aria-labelledby="attachment-workbench-heading">
      <header className="section-header">
        <div>
          <span className="eyebrow">Deterministic client presentation</span>
          <h1 id="attachment-workbench-heading">Artifact attachments</h1>
        </div>
        <span className="attachment-bound">12-item display ceiling</span>
      </header>
      <p className="artifact-bound-summary">
        Typed attachment fixtures only · no production attachment facts · no media prefetched
      </p>
      <div className="attachment-layout">
        <div className="attachment-columns">
          <AttachmentList
            label="Transcript attachments"
            items={attachmentScenario}
            selectedId={selectedId}
            onSelect={(artifact) => setSelectedId(artifact.id)}
          />
          <AttachmentList
            label="Composer attachments"
            items={composerItems}
            selectedId={selectedId}
            onSelect={(artifact) => setSelectedId(artifact.id)}
            onRemove={(artifact) => {
              setComposerItems((items) => items.filter((item) => item.id !== artifact.id))
              if (selectedId === artifact.id) setSelectedId(null)
            }}
          />
        </div>
        <section className="attachment-preview" aria-label="Selected attachment preview">
          <header>
            <span className="eyebrow">Bounded preview</span>
            <strong>{selected?.displayName ?? 'No attachment selected'}</strong>
          </header>
          {selected ? (
            <ArtifactRenderer artifact={selected} remoteMediaPolicy={remoteMedia} />
          ) : (
            <p className="attachment-empty">Select an attachment to inspect its typed renderer.</p>
          )}
        </section>
      </div>
    </section>
  )
}
