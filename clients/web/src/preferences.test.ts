import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  BROWSER_PREFERENCES_KEY,
  decodeBrowserPreferences,
  defaultBrowserPreferences,
  loadBrowserPreferences,
} from './preferences'

afterEach(() => vi.unstubAllGlobals())

describe('browser preferences', () => {
  it('fails closed to defaults for an unrelated stored value', () => {
    expect(decodeBrowserPreferences('not-an-object')).toEqual(defaultBrowserPreferences)
  })

  it('rejects a non-current record as a whole', () => {
    const stored = {
      layout: 'focus',
      density: 'comfortable',
      detail: 'full',
      theme: 'light',
      keyOverrides: {},
    } as const

    expect(decodeBrowserPreferences(stored)).toEqual(defaultBrowserPreferences)
  })

  it('accepts only the exact current record', () => {
    const stored = {
      layout: 'focus',
      density: 'comfortable',
      detail: 'full',
      theme: 'light',
    } as const

    expect(decodeBrowserPreferences(stored)).toEqual(stored)
  })

  it('falls back when browser storage access throws', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new DOMException('denied', 'SecurityError')
      },
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
    expect(BROWSER_PREFERENCES_KEY).toBe('signalbox.web.preferences.v1')
  })
})
