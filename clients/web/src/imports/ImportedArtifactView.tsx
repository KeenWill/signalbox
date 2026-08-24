import { ArtifactRenderer } from '../features/artifacts/ArtifactRenderer'
import type { ArtifactItem } from '../features/artifacts/artifactTypes'
import type { WebImportedEntry } from '../generated/web-contract.mjs'

const contentKindLabel = (kind: WebImportedEntry['content_kind']): string =>
  kind.replaceAll('_', ' ')

export const projectImportedEntryArtifact = (entry: WebImportedEntry): ArtifactItem => {
  const identity = {
    id: entry.frontier.imported_entry_id,
    displayName: `Imported entry ${BigInt(entry.frontier.position).toLocaleString()}`,
  }
  if (entry.content_kind !== 'text') {
    return {
      ...identity,
      kind: 'blocked',
      attemptedKind: `imported ${contentKindLabel(entry.content_kind)}`,
      reason: 'No typed renderer is available for this imported content kind.',
    }
  }
  if (!entry.text) {
    throw new TypeError('imported text entry is missing typed text evidence')
  }
  if (entry.text.kind === 'not_attested') {
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
  return {
    ...identity,
    kind: 'text',
    content: entry.text.leading_text,
    characterCount: [...entry.text.leading_text].length,
  }
}

const sourceBoundLabel = (entry: WebImportedEntry): string => {
  if (entry.text?.kind !== 'attested') return 'No attested text payload'
  return entry.text.completeness === 'truncated'
    ? 'Server-bounded source prefix'
    : 'Complete attested source text'
}

export function ImportedArtifactView({
  entry,
  commandContext,
}: {
  entry: WebImportedEntry | null
  commandContext: import('../commands').CommandContext
}) {
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
        <ArtifactRenderer
          artifact={projectImportedEntryArtifact(entry)}
          commandContext={commandContext}
        />
      ) : (
        <p className="imports-state">Select an imported source entry to inspect its typed view.</p>
      )}
    </section>
  )
}
