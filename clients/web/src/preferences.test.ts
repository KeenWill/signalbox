import { describe, expect, it, vi } from 'vitest'
import {
  BROWSER_PREFERENCES_KEY,
  decodeBrowserPreferences,
  defaultBrowserPreferences,
  loadBrowserPreferences,
  MAX_BROWSER_PREFERENCES_BYTES,
  paneSizeBounds,
  saveBrowserPreferences,
} from './preferences'

const restoreGlobalProperty = (property: string, descriptor: PropertyDescriptor | undefined) => {
  if (descriptor) {
    Object.defineProperty(globalThis, property, descriptor)
  } else {
    Reflect.deleteProperty(globalThis, property)
  }
}

describe('browser preferences', () => {
  it('fails closed to defaults for an unrelated stored value', () => {
    expect(() => decodeBrowserPreferences('not-an-object')).toThrow(TypeError)
  })

  it('clamps pane sizes for the exact current schema', () => {
    const stored = {
      layout: 'workbench',
      density: 'comfortable',
      detail: 'results',
      theme: 'light',
      paneSizes: { navigation: -50, inspector: 50_000 },
    } as const
    const decoded = decodeBrowserPreferences(stored)

    expect(decoded.layout).toBe(stored.layout)
    expect(decoded.density).toBe(stored.density)
    expect(decoded.paneSizes).toEqual({
      navigation: paneSizeBounds.navigation.minimum,
      inspector: paneSizeBounds.inspector.maximum,
    })
  })

  it('rejects obsolete preference fields atomically', () => {
    const obsolete = { ...defaultBrowserPreferences, remoteMedia: 'ask' }

    expect(() => decodeBrowserPreferences(obsolete)).toThrow(TypeError)
  })

  it('rejects oversized stored preferences before parsing', () => {
    const storage = {
      getItem: (key: string) =>
        key === BROWSER_PREFERENCES_KEY ? 'x'.repeat(MAX_BROWSER_PREFERENCES_BYTES + 1) : null,
      setItem: () => undefined,
    }
    vi.stubGlobal('localStorage', storage)

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)

    vi.unstubAllGlobals()
  })

  it('falls back when browser storage cannot be read or written', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new DOMException('denied')
      },
      setItem: () => {
        throw new DOMException('denied')
      },
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
    expect(() => saveBrowserPreferences(defaultBrowserPreferences)).not.toThrow()

    vi.unstubAllGlobals()
  })

  it('falls back when the browser storage getter itself is denied', () => {
    const originalDescriptor = Object.getOwnPropertyDescriptor(globalThis, 'localStorage')
    Object.defineProperty(globalThis, 'localStorage', {
      configurable: true,
      get: () => {
        throw new DOMException('denied')
      },
    })

    try {
      expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
      expect(() => saveBrowserPreferences(defaultBrowserPreferences)).not.toThrow()
    } finally {
      restoreGlobalProperty('localStorage', originalDescriptor)
    }
  })
})
