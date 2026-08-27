import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  applyStoredVisualPreferences,
  BROWSER_PREFERENCES_KEY,
  decodeBrowserPreferences,
  defaultBrowserPreferences,
  loadBrowserPreferences,
  MAX_BROWSER_PREFERENCES_BYTES,
  MAX_LOGICAL_POSITION_KEY_BYTES,
  MAX_LOGICAL_POSITION_VALUE_BYTES,
  MAX_SAVED_LOGICAL_POSITIONS,
  saveBrowserPreferences,
  serializeBrowserPreferences,
} from './preferences'

const originalLocalStorageDescriptor = Object.getOwnPropertyDescriptor(globalThis, 'localStorage')

const restoreLocalStorageDescriptor = () => {
  if (originalLocalStorageDescriptor) {
    Object.defineProperty(globalThis, 'localStorage', originalLocalStorageDescriptor)
  } else {
    Reflect.deleteProperty(globalThis, 'localStorage')
  }
}

afterEach(() => {
  vi.unstubAllGlobals()
  restoreLocalStorageDescriptor()
})

const oversizedLogicalPositionsFixture = () =>
  Object.fromEntries(
    Array.from({ length: MAX_SAVED_LOGICAL_POSITIONS }, (_, index) => [
      `session-${index}`,
      '\0'.repeat(MAX_LOGICAL_POSITION_VALUE_BYTES),
    ]),
  )

describe('browser preferences', () => {
  it('fails closed to defaults for an unrelated stored value', () => {
    expect(() => decodeBrowserPreferences('not-an-object')).toThrow('preferences must be an object')
  })

  it('rejects partial and unknown preference schemas atomically', () => {
    const stored = {
      layout: 'dashboard',
      density: 'comfortable',
      paneSizes: { navigation: -50, inspector: 50_000 },
      remoteMedia: 'proxy',
    } as const
    expect(() => decodeBrowserPreferences(stored)).toThrow(
      'preferences must match the current exact schema',
    )
  })

  it('bounds retained logical positions', () => {
    const lastLogicalPositions = Object.fromEntries(
      Array.from({ length: MAX_SAVED_LOGICAL_POSITIONS + 3 }, (_, index) => [
        `session-${index}`,
        String(index + 1),
      ]),
    )

    const decoded = decodeBrowserPreferences({
      ...defaultBrowserPreferences,
      lastLogicalPositions,
    })

    expect(Object.keys(decoded.lastLogicalPositions)).toHaveLength(MAX_SAVED_LOGICAL_POSITIONS)
  })

  it('loads defaults atomically when the stored schema is partial', () => {
    vi.stubGlobal('localStorage', {
      getItem: (key: string) =>
        key === BROWSER_PREFERENCES_KEY ? JSON.stringify({ layout: 'focus' }) : null,
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
  })

  it('rejects an oversized stored payload before parsing it', () => {
    vi.stubGlobal('localStorage', {
      getItem: (key: string) =>
        key === BROWSER_PREFERENCES_KEY ? 'x'.repeat(MAX_BROWSER_PREFERENCES_BYTES + 1) : null,
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
  })

  it('does not persist preferences above the serialized byte ceiling', () => {
    const setItem = vi.fn()
    vi.stubGlobal('localStorage', { setItem })
    const oversized = {
      ...defaultBrowserPreferences,
      lastLogicalPositions: oversizedLogicalPositionsFixture(),
    }

    expect(serializeBrowserPreferences(oversized)).toBeNull()
    saveBrowserPreferences(oversized)
    expect(setItem).not.toHaveBeenCalled()
  })

  it('treats browser storage access failures as optional', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new DOMException('blocked', 'SecurityError')
      },
      setItem: () => {
        throw new DOMException('full', 'QuotaExceededError')
      },
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
    expect(() => saveBrowserPreferences(defaultBrowserPreferences)).not.toThrow()
  })

  it('guards access to a throwing browser storage getter', () => {
    Object.defineProperty(globalThis, 'localStorage', {
      configurable: true,
      get: () => {
        throw new DOMException('blocked', 'SecurityError')
      },
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
    expect(() => saveBrowserPreferences(defaultBrowserPreferences)).not.toThrow()
  })

  it('rejects logical-position keys and values above their UTF-8 byte ceilings', () => {
    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        lastLogicalPositions: { ['é'.repeat(MAX_LOGICAL_POSITION_KEY_BYTES)]: '7' },
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values are out of bounds')

    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        lastLogicalPositions: { session: 'é'.repeat(MAX_LOGICAL_POSITION_VALUE_BYTES) },
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values are out of bounds')
  })

  it('rejects malformed saved logical positions atomically', () => {
    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        lastLogicalPositions: { malformed: 'not-a-position' },
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values are out of bounds')

    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        lastLogicalPositions: { zero: '0' },
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values are out of bounds')

    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        lastLogicalPositions: { overflow: '18446744073709551616' },
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values are out of bounds')
  })

  it('accepts logical-position keys exactly at their byte ceiling', () => {
    const decoded = decodeBrowserPreferences({
      ...defaultBrowserPreferences,
      lastLogicalPositions: {
        ['é'.repeat(MAX_LOGICAL_POSITION_KEY_BYTES / 2)]: '18446744073709551615',
      },
    })

    expect(Object.keys(decoded.lastLogicalPositions)).toHaveLength(1)
  })

  it('rejects logical-position keys with unordered plain-object key semantics', () => {
    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        lastLogicalPositions: { 1: '7' },
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values are out of bounds')

    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        lastLogicalPositions: JSON.parse('{"__proto__":"7"}'),
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values are out of bounds')
  })

  const stubDocumentDataset = () => {
    const dataset: Record<string, string> = {}
    vi.stubGlobal('document', { documentElement: { dataset } })
    return dataset
  }

  it('paints the stored theme and density before the application mounts', () => {
    const stored = { ...defaultBrowserPreferences, theme: 'light', density: 'comfortable' } as const
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => (key === BROWSER_PREFERENCES_KEY ? JSON.stringify(stored) : null),
      setItem: () => undefined,
    })
    const dataset = stubDocumentDataset()

    applyStoredVisualPreferences()

    expect(dataset.theme).toBe('light')
    expect(dataset.density).toBe('comfortable')
  })

  it('paints the default presentation when nothing is stored', () => {
    vi.stubGlobal('localStorage', { getItem: () => null, setItem: () => undefined })
    const dataset = stubDocumentDataset()

    applyStoredVisualPreferences()

    expect(dataset.theme).toBe(defaultBrowserPreferences.theme)
    expect(dataset.density).toBe(defaultBrowserPreferences.density)
  })
})
