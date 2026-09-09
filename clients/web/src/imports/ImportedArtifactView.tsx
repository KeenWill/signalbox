import type { CommandContext } from '../commands'
import { ArtifactRenderer } from '../features/artifacts/ArtifactRenderer'
import type { ArtifactItem } from '../features/artifacts/artifactTypes'
import type { WebImportedEntry } from '../generated/web-contract.mjs'
import { enumLabel } from '../labels'

const contentKindLabel = (kind: WebImportedEntry['content_kind']): string => enumLabel(kind)

export const projectImportedEntryArtifact = (entry: WebImportedEntry): ArtifactItem => {
  const identity = {
    id: entry.frontier.imported_entry_id,
    displayName: `Imported entry ${entry.frontier.position.toLocaleString()}`,
  }
  if (entry.content_kind !== 'text') {
    if (entry.text !== null && entry.text !== undefined) {
      throw new TypeError('non-text imported entry carries text evidence')
    }
    return {
      ...identity,
      kind: 'blocked',
      attemptedKind: `Imported ${contentKindLabel(entry.content_kind).toLowerCase()}`,
      reason: 'No preview for this content.',
    }
  }
  if (!entry.text) {
    throw new TypeError('imported text entry is missing typed text evidence')
  }
  if (entry.text.kind === 'not_attested') {
    return {
      ...identity,
      kind: 'blocked',
      attemptedKind: 'Imported text',
      reason: 'Text unknown.',
    }
  }
  if (entry.text.kind === 'attested_absent') {
    return {
      ...identity,
      kind: 'blocked',
      attemptedKind: 'Imported text',
      reason: 'Text absent.',
    }
  }
  return {
    ...identity,
    kind: 'text',
    content: entry.text.leading_text,
    characterCount: [...entry.text.leading_text].length,
    sourceComplete: entry.text.completeness === 'complete',
  }
}

export function ImportedArtifactView({
  entry,
  commandContext,
}: {
  entry: WebImportedEntry | null
  commandContext: CommandContext
}) {
  return (
    <section className="import-artifact-view" aria-label="Imported entry">
      {entry ? (
        <ArtifactRenderer
          artifact={projectImportedEntryArtifact(entry)}
          commandContext={commandContext}
        />
      ) : (
        <p className="imports-state">No entry selected.</p>
      )}
    </section>
  )
}
