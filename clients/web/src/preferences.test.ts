import { describe, expect, it, vi } from 'vitest'
import {
  decodeBrowserPreferences,
  defaultBrowserPreferences,
  loadBrowserPreferences,
  paneSizeBounds,
  saveBrowserPreferences,
} from './preferences'

describe('browser preferences', () => {
  it('fails closed to defaults for an unrelated stored value', () => {
    expect(decodeBrowserPreferences('not-an-object')).toEqual(defaultBrowserPreferences)
  })

  it('clamps pane sizes and rejects unknown closed variants', () => {
    const stored = {
      layout: 'dashboard',
      density: 'comfortable',
      paneSizes: { navigation: -50, inspector: 50_000 },
    } as const
    const decoded = decodeBrowserPreferences(stored)

    expect(decoded.layout).toBe(defaultBrowserPreferences.layout)
    expect(decoded.density).toBe(stored.density)
    expect(decoded.paneSizes).toEqual({
      navigation: paneSizeBounds.navigation.minimum,
      inspector: paneSizeBounds.inspector.maximum,
    })
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
      if (originalDescriptor) {
        Object.defineProperty(globalThis, 'localStorage', originalDescriptor)
      } else {
        Reflect.deleteProperty(globalThis, 'localStorage')
      }
    }
  })
})
