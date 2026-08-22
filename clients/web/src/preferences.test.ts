import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  applyPresentationPreferences,
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

  it('falls back when looking up the browser storage global throws', () => {
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'localStorage')
    Object.defineProperty(globalThis, 'localStorage', {
      configurable: true,
      get: () => {
        throw new DOMException('denied', 'SecurityError')
      },
    })

    try {
      expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
    } finally {
      if (descriptor) Object.defineProperty(globalThis, 'localStorage', descriptor)
      else Reflect.deleteProperty(globalThis, 'localStorage')
    }
  })

  it('applies decoded presentation settings synchronously', () => {
    const root = { dataset: {} } as unknown as HTMLElement

    applyPresentationPreferences({ density: 'comfortable', theme: 'light' }, root)

    expect(root.dataset).toEqual({ density: 'comfortable', theme: 'light' })
  })
})
