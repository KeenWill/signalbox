import { describe, expect, it } from 'vitest'
import type { WebImportedEntry } from '../generated/web-contract.mjs'
import { projectImportedEntryArtifact } from './ImportedArtifactView'

const attestedText = 'Attested source evidence.'

const importedText: WebImportedEntry = {
  frontier: {
    imported_conversation_id: '00000000-0000-7000-8000-000000000001',
    imported_entry_id: '00000000-0000-7000-8000-000000000002',
    position: '8',
  },
  raw_record_position: '3',
  record_entry_position: '2',
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
      characterCount: [...attestedText].length,
    })
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
})
