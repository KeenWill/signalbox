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
    } as const
    const decoded = decodeBrowserPreferences(stored)

    expect(decoded.layout).toBe(defaultBrowserPreferences.layout)
    expect(decoded.density).toBe(stored.density)
    expect(decoded.paneSizes).toEqual({ navigation: 160, inspector: 480 })
  })

  it('bounds retained positions and future key overrides', () => {
    const lastLogicalPositions = Object.fromEntries(
      Array.from({ length: MAX_SAVED_LOGICAL_POSITIONS + 3 }, (_, index) => [
        `00000000-0000-0000-0000-${String(index).padStart(12, '0')}`,
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

  it('rejects malformed remembered session identities and timeline addresses', () => {
    const validSession = '00000000-0000-0000-0000-000000000991'
    const decoded = decodeBrowserPreferences({
      lastLogicalPositions: {
        [validSession]: '42',
        'not-a-session': '42',
        '00000000-0000-0000-0000-000000000992': 'cursor-1',
        '00000000-0000-0000-0000-000000000993': '18446744073709551616',
      },
    })

    expect(decoded.lastLogicalPositions).toEqual({ [validSession]: '42' })
  })

  it('falls back when browser storage reads are unavailable', () => {
    vi.stubGlobal('localStorage', {
      getItem: vi.fn(() => {
        throw new DOMException('denied', 'SecurityError')
      }),
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
  })

  it('keeps in-memory preferences when browser storage writes are unavailable', () => {
    vi.stubGlobal('localStorage', {
      setItem: vi.fn(() => {
        throw new DOMException('quota', 'QuotaExceededError')
      }),
    })

    expect(() => saveBrowserPreferences(defaultBrowserPreferences)).not.toThrow()
  })
})
