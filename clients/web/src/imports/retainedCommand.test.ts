import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { WebImportContinuationRequest } from '../generated/web-contract.mjs'
import { loadRetainedCommand, storeRetainedCommand } from './retainedCommand'

const command = (): WebImportContinuationRequest => ({
  command_id: '00000000-0000-7000-8000-000000000001',
  frontier: {
    imported_conversation_id: '00000000-0000-7000-8000-000000000002',
    imported_entry_id: '00000000-0000-7000-8000-000000000003',
    position: 7,
  },
  relationship: 'resume',
  initial_model_selection: {
    kind: 'direct',
    selection_id: '00000000-0000-7000-8000-000000000004',
  },
})

beforeEach(() => {
  const values = new Map<string, string>()
  vi.stubGlobal('window', {
    sessionStorage: {
      getItem: (key: string) => values.get(key) ?? null,
      removeItem: (key: string) => values.delete(key),
      setItem: (key: string, value: string) => values.set(key, value),
    },
  })
})

afterEach(() => vi.unstubAllGlobals())

describe('retained import continuation commands', () => {
  it('restores the exact immutable request in its original scope', () => {
    const expected = command()

    storeRetainedCommand('production', expected)

    expect(loadRetainedCommand('production')).toEqual(expected)
  })

  it('keeps production and scenario commands separate', () => {
    storeRetainedCommand('production', command())

    expect(loadRetainedCommand('scenario')).toBeNull()
  })

  it('evicts malformed stored commands', () => {
    window.sessionStorage.setItem('signalbox.import-continuation.production', '{')

    expect(loadRetainedCommand('production')).toBeNull()
    expect(window.sessionStorage.getItem('signalbox.import-continuation.production')).toBeNull()
  })

  it('evicts stored commands beyond the encoded byte ceiling', () => {
    window.sessionStorage.setItem('signalbox.import-continuation.production', 'x'.repeat(4097))

    expect(loadRetainedCommand('production')).toBeNull()
    expect(window.sessionStorage.getItem('signalbox.import-continuation.production')).toBeNull()
  })
})
