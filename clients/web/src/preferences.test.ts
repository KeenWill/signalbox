import { describe, expect, it, vi } from 'vitest'
import {
  decodeBrowserPreferences,
  defaultBrowserPreferences,
  loadBrowserPreferences,
  MAX_KEY_OVERRIDES,
  MAX_SAVED_LOGICAL_POSITIONS,
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
    expect(decoded.paneSizes).toEqual({ navigation: 160, inspector: 480 })
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

  it('bounds retained positions and future key overrides', () => {
    const lastLogicalPositions = Object.fromEntries(
      Array.from({ length: MAX_SAVED_LOGICAL_POSITIONS + 3 }, (_, index) => [
        `session-${index}`,
        `cursor-${index}`,
      ]),
    )
    const keyOverrides = Object.fromEntries(
      Array.from({ length: MAX_KEY_OVERRIDES + 2 }, (_, index) => [
        `command-${index}`,
        `key-${index}`,
      ]),
    )

    const decoded = decodeBrowserPreferences({ lastLogicalPositions, keyOverrides })

    expect(Object.keys(decoded.lastLogicalPositions)).toHaveLength(MAX_SAVED_LOGICAL_POSITIONS)
    expect(Object.keys(decoded.keyOverrides)).toHaveLength(MAX_KEY_OVERRIDES)
  })
})
