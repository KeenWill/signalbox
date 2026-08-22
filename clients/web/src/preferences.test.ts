import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  decodeBrowserPreferences,
  defaultBrowserPreferences,
  loadBrowserPreferences,
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
      remoteMedia: 'allow',
    } as const
    const decoded = decodeBrowserPreferences(stored)

    expect(decoded.layout).toBe(defaultBrowserPreferences.layout)
    expect(decoded.density).toBe(stored.density)
    expect(decoded.paneSizes).toEqual({ navigation: 160, inspector: 480 })
    expect(decoded).not.toHaveProperty('remoteMedia')
  })

  it('bounds retained positions and ignores unsupported key overrides', () => {
    const lastLogicalPositions = Object.fromEntries(
      Array.from({ length: MAX_SAVED_LOGICAL_POSITIONS + 3 }, (_, index) => [
        `session-${index}`,
        String(index + 1),
      ]),
    )
    const decoded = decodeBrowserPreferences({
      lastLogicalPositions,
      keyOverrides: { 'selection.next': 'n' },
    })

    expect(Object.keys(decoded.lastLogicalPositions)).toHaveLength(MAX_SAVED_LOGICAL_POSITIONS)
    expect(decoded).not.toHaveProperty('keyOverrides')
  })

  it('discards malformed saved logical positions', () => {
    const decoded = decodeBrowserPreferences({
      lastLogicalPositions: {
        valid: '42',
        zero: '0',
        malformed: 'not-a-position',
        overflow: '18446744073709551616',
      },
    })

    expect(decoded.lastLogicalPositions).toEqual({ valid: '42' })
  })

  it('falls back to in-memory preferences when browser storage is unavailable', () => {
    vi.stubGlobal('localStorage', {
      getItem: vi.fn(() => {
        throw new DOMException('blocked', 'SecurityError')
      }),
      setItem: vi.fn(() => {
        throw new DOMException('full', 'QuotaExceededError')
      }),
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
    expect(() => saveBrowserPreferences(defaultBrowserPreferences)).not.toThrow()
  })
})
