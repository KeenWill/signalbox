import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  decodeBrowserPreferences,
  defaultBrowserPreferences,
  loadBrowserPreferences,
  MAX_KEY_OVERRIDES,
  MAX_SAVED_LOGICAL_POSITIONS,
  saveBrowserPreferences,
} from './preferences'

afterEach(() => vi.unstubAllGlobals())

describe('browser preferences', () => {
  it('fails closed to defaults for an unrelated stored value', () => {
    expect(decodeBrowserPreferences('not-an-object')).toEqual(defaultBrowserPreferences)
  })

  it('clamps pane sizes and rejects unknown closed variants', () => {
    const stored = {
      layout: 'dashboard',
      density: 'comfortable',
      paneSizes: { navigation: -50, inspector: 50_000 },
      remoteMedia: 'proxy',
    } as const
    const decoded = decodeBrowserPreferences(stored)

    expect(decoded.layout).toBe(defaultBrowserPreferences.layout)
    expect(decoded.density).toBe(stored.density)
    expect(decoded.paneSizes).toEqual({ navigation: 160, inspector: 480 })
    expect(decoded.remoteMedia).toBe(defaultBrowserPreferences.remoteMedia)
  })

  it('bounds retained positions and future key overrides', () => {
    const lastLogicalPositions = Object.fromEntries(
      Array.from({ length: MAX_SAVED_LOGICAL_POSITIONS + 3 }, (_, index) => [
        `session-${index}`,
        String(index + 1),
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

  it('drops malformed and out-of-range restored logical positions', () => {
    const decoded = decodeBrowserPreferences({
      lastLogicalPositions: {
        valid: '42',
        zero: '0',
        malformed: 'timeline:12',
        overflow: '18446744073709551616',
      },
    })

    expect(decoded.lastLogicalPositions).toEqual({ valid: '42' })
  })

  it('falls back to defaults when browser storage reads throw', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('storage denied')
      },
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
  })

  it('keeps browser storage write failures non-fatal', () => {
    vi.stubGlobal('localStorage', {
      setItem: () => {
        throw new Error('storage denied')
      },
    })

    expect(() => saveBrowserPreferences(defaultBrowserPreferences)).not.toThrow()
  })
})
