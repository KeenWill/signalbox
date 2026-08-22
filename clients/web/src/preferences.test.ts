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
    } as const
    const decoded = decodeBrowserPreferences(stored)

    expect(decoded.layout).toBe(defaultBrowserPreferences.layout)
    expect(decoded.density).toBe(stored.density)
    expect(decoded.paneSizes).toEqual({ navigation: 160, inspector: 480 })
  })

  it('bounds retained logical positions', () => {
    const lastLogicalPositions = Object.fromEntries(
      Array.from({ length: MAX_SAVED_LOGICAL_POSITIONS + 3 }, (_, index) => [
        `00000000-0000-0000-0000-${index.toString(16).padStart(12, '0')}`,
        String(index + 1),
      ]),
    )
    const decoded = decodeBrowserPreferences({ lastLogicalPositions })

    expect(Object.keys(decoded.lastLogicalPositions)).toHaveLength(MAX_SAVED_LOGICAL_POSITIONS)
  })

  it('discards malformed persisted session positions', () => {
    const validSession = '00000000-0000-0000-0000-000000000991'
    const decoded = decodeBrowserPreferences({
      lastLogicalPositions: {
        [validSession]: '42',
        '00000000-0000-0000-0000-00000000099A': '43',
        'not-a-session': '44',
        '00000000-0000-0000-0000-000000000992': '0',
        '00000000-0000-0000-0000-000000000993': '01',
        '00000000-0000-0000-0000-000000000994': '18446744073709551616',
      },
    })

    expect(decoded.lastLogicalPositions).toEqual({ [validSession]: '42' })
  })

  it('falls back when browser storage access is unavailable', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new DOMException('denied', 'SecurityError')
      },
      setItem: () => {
        throw new DOMException('full', 'QuotaExceededError')
      },
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
    expect(() => saveBrowserPreferences(defaultBrowserPreferences)).not.toThrow()
  })
})
