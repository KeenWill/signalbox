import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  BROWSER_PREFERENCES_KEY,
  decodeBrowserPreferences,
  defaultBrowserPreferences,
  loadBrowserPreferences,
  MAX_BROWSER_PREFERENCES_BYTES,
  MAX_KEY_OVERRIDE_KEY_BYTES,
  MAX_KEY_OVERRIDE_VALUE_BYTES,
  MAX_KEY_OVERRIDES,
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

  it('bounds retained key overrides', () => {
    const keyOverrides = Object.fromEntries(
      Array.from({ length: MAX_KEY_OVERRIDES + 2 }, (_, index) => [
        `command-${index}`,
        `key-${index}`,
      ]),
    )

    const decoded = decodeBrowserPreferences({
      ...defaultBrowserPreferences,
      keyOverrides,
    })

    expect(Object.keys(decoded.keyOverrides)).toHaveLength(MAX_KEY_OVERRIDES)
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

  it('builds the oversized logical-position fixture at its declared bounds', () => {
    const positions = oversizedLogicalPositionsFixture()

    expect(Object.keys(positions)).toHaveLength(MAX_SAVED_LOGICAL_POSITIONS)
    expect(positions['session-0']).toBe('\0'.repeat(MAX_LOGICAL_POSITION_VALUE_BYTES))
    expect(positions[`session-${MAX_SAVED_LOGICAL_POSITIONS - 1}`]).toBeDefined()
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
        lastLogicalPositions: { ['é'.repeat(MAX_LOGICAL_POSITION_KEY_BYTES)]: 'cursor' },
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values exceed their byte limits')

    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        lastLogicalPositions: { session: 'é'.repeat(MAX_LOGICAL_POSITION_VALUE_BYTES) },
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values exceed their byte limits')
  })

  it('accepts canonical logical positions with keys at their UTF-8 byte ceiling', () => {
    const decoded = decodeBrowserPreferences({
      ...defaultBrowserPreferences,
      lastLogicalPositions: {
        ['é'.repeat(MAX_LOGICAL_POSITION_KEY_BYTES / 2)]: '18446744073709551615',
      },
    })

    expect(Object.keys(decoded.lastLogicalPositions)).toHaveLength(1)
  })

  it('rejects noncanonical and out-of-range persisted logical positions', () => {
    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        lastLogicalPositions: { session: 'cursor' },
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values exceed their byte limits')

    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        lastLogicalPositions: { session: '0' },
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values exceed their byte limits')

    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        lastLogicalPositions: { session: '01' },
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values exceed their byte limits')

    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        lastLogicalPositions: { session: '18446744073709551616' },
      }),
    ).toThrow('preferences.lastLogicalPositions keys or values exceed their byte limits')
  })

  it('rejects key-override keys and values above their UTF-8 byte ceilings', () => {
    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        keyOverrides: { ['é'.repeat(MAX_KEY_OVERRIDE_KEY_BYTES)]: 'Shift+K' },
      }),
    ).toThrow('preferences.keyOverrides keys or values exceed their byte limits')

    expect(() =>
      decodeBrowserPreferences({
        ...defaultBrowserPreferences,
        keyOverrides: { command: 'é'.repeat(MAX_KEY_OVERRIDE_VALUE_BYTES) },
      }),
    ).toThrow('preferences.keyOverrides keys or values exceed their byte limits')
  })
})
