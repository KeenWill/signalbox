import { ArtifactRenderer } from '../features/artifacts/ArtifactRenderer'
import type { ArtifactItem } from '../features/artifacts/artifactTypes'
import type { WebImportedEntry } from '../generated/web-contract.mjs'

const contentKindLabel = (kind: WebImportedEntry['content_kind']): string =>
  kind.replaceAll('_', ' ')

export const projectImportedEntryArtifact = (entry: WebImportedEntry): ArtifactItem => {
  const identity = {
    id: entry.frontier.imported_entry_id,
    displayName: `Imported entry ${entry.frontier.position.toLocaleString()}`,
  }
  if (entry.content_kind !== 'text') {
    return {
      ...identity,
      kind: 'committed_unimplemented',
      attemptedKind: `imported ${contentKindLabel(entry.content_kind)}`,
    }
  }
  if (!entry.text || entry.text.kind === 'not_attested') {
    return {
      ...identity,
      kind: 'blocked',
      attemptedKind: 'imported text',
      reason: 'The source did not attest text for this entry. No content was inferred.',
    }
  }
  if (entry.text.kind === 'attested_absent') {
    return {
      ...identity,
      kind: 'blocked',
      attemptedKind: 'imported text',
      reason: 'The source explicitly attests that text is absent. No content was inferred.',
    }
  }
  return { ...identity, kind: 'text', content: entry.text.leading_text }
}

const sourceBoundLabel = (entry: WebImportedEntry): string => {
  if (entry.text?.kind !== 'attested') return 'No attested text payload'
  return entry.text.completeness === 'truncated'
    ? 'Server-bounded source prefix'
    : 'Complete attested source text'
}

export function ImportedArtifactView({ entry }: { entry: WebImportedEntry | null }) {
  return (
    <section className="import-artifact-view" aria-labelledby="import-artifact-heading">
      <header>
        <div>
          <span className="eyebrow">Typed artifact view</span>
          <h3 id="import-artifact-heading">Selected imported evidence</h3>
        </div>
        {entry && <small>{sourceBoundLabel(entry)}</small>}
      </header>
      {entry ? (
        <ArtifactRenderer artifact={projectImportedEntryArtifact(entry)} />
      ) : (
        <p className="imports-state">Select an imported source entry to inspect its typed view.</p>
      )}
    </section>
  )
}
