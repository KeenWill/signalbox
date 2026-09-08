import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { Provider } from 'react-redux'
import { describe, expect, it } from 'vitest'
import { ArtifactRenderer } from '../features/artifacts/ArtifactRenderer'
import type { WebImportedEntry } from '../generated/web-contract.mjs'
import { store } from '../state'
import { projectImportedEntryArtifact } from './ImportedArtifactView'

const attestedText = 'A😀B'

const importedText: WebImportedEntry = {
  frontier: {
    imported_conversation_id: '00000000-0000-7000-8000-000000000001',
    imported_entry_id: '00000000-0000-7000-8000-000000000002',
    position: 8,
  },
  raw_record_position: 3,
  record_entry_position: 2,
  source_speaker: 'assistant',
  content_kind: 'text',
  text: { kind: 'attested', leading_text: attestedText, completeness: 'complete' },
}

describe('imported artifact projection', () => {
  it('projects only attested imported text as renderable content', () => {
    const artifact = projectImportedEntryArtifact(importedText)

    expect(artifact).toEqual({
      id: importedText.frontier.imported_entry_id,
      displayName: 'Imported entry 8',
      kind: 'text',
      content: attestedText,
      characterCount: 3,
      sourceComplete: true,
    })
  })

  it('preserves a server-truncated source projection as incomplete', () => {
    const artifact = projectImportedEntryArtifact({
      ...importedText,
      text: { kind: 'attested', leading_text: attestedText, completeness: 'truncated' },
    })

    expect(artifact).toMatchObject({
      kind: 'text',
      content: attestedText,
      sourceComplete: false,
    })
  })

  it('renders a truncated imported prefix without claiming complete content', () => {
    const artifact = projectImportedEntryArtifact({
      ...importedText,
      text: { kind: 'attested', leading_text: attestedText, completeness: 'truncated' },
    })
    const markup = renderToStaticMarkup(
      createElement(
        Provider,
        // biome-ignore lint/correctness/noChildrenProp: ProviderProps requires children even with a child argument.
        { store, children: null },
        createElement(ArtifactRenderer, {
          artifact,
          commandContext: {
            dispatch: store.dispatch,
            getState: store.getState,
            timelineIds: [],
            artifactPreviewIds: [],
            artifactOriginalIds: [],
            focusTimeline: () => undefined,
          },
        }),
      ),
    )
    expect(markup).toContain(
      'Server-truncated source prefix shown; additional source content is not loaded.',
    )
    expect(markup).not.toContain('Complete bounded content shown')
  })

  it('keeps an imported document committed-unimplemented without a blob descriptor', () => {
    const documentEntry: WebImportedEntry = {
      ...importedText,
      content_kind: 'document',
      text: null,
    }

    const artifact = projectImportedEntryArtifact(documentEntry)

    expect(artifact).toEqual({
      id: documentEntry.frontier.imported_entry_id,
      displayName: 'Imported entry 8',
      kind: 'blocked',
      attemptedKind: 'imported document',
      reason: 'No typed renderer is available for this imported content kind.',
    })
  })

  it('blocks imported text when the source did not attest its content', () => {
    const unattestedEntry: WebImportedEntry = {
      ...importedText,
      text: { kind: 'not_attested' },
    }

    const artifact = projectImportedEntryArtifact(unattestedEntry)

    expect(artifact).toEqual({
      id: unattestedEntry.frontier.imported_entry_id,
      displayName: 'Imported entry 8',
      kind: 'blocked',
      attemptedKind: 'imported text',
      reason: 'The source did not attest text for this entry. No content was inferred.',
    })
  })

  it('rejects text entries that omit typed text evidence', () => {
    expect(() => projectImportedEntryArtifact({ ...importedText, text: null })).toThrow(
      'missing typed text evidence',
    )
  })

  it('rejects text evidence attached to a non-text entry', () => {
    expect(() =>
      projectImportedEntryArtifact({ ...importedText, content_kind: 'document' }),
    ).toThrow('non-text imported entry carries text evidence')
  })
})
