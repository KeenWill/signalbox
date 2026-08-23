import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  decodeBrowserPreferences,
  defaultBrowserPreferences,
  loadBrowserPreferences,
  MAX_KEY_OVERRIDES,
  MAX_PREFERENCE_RECORD_KEY_BYTES,
  MAX_PREFERENCE_RECORD_VALUE_BYTES,
  MAX_PREFERENCE_STORAGE_BYTES,
  MAX_SAVED_LOGICAL_POSITIONS,
  saveBrowserPreferences,
} from './preferences'

const originalLocalStorageDescriptor = Object.getOwnPropertyDescriptor(globalThis, 'localStorage')

const restoreLocalStorageDescriptor = () => {
  if (originalLocalStorageDescriptor === undefined) {
    Reflect.deleteProperty(globalThis, 'localStorage')
    return
  }
  Object.defineProperty(globalThis, 'localStorage', originalLocalStorageDescriptor)
}

afterEach(() => {
  vi.unstubAllGlobals()
  restoreLocalStorageDescriptor()
})

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

  it('drops preference records with oversized UTF-8 keys', () => {
    const oversizedKey = 'é'.repeat(MAX_PREFERENCE_RECORD_KEY_BYTES / 2 + 1)

    const decoded = decodeBrowserPreferences({
      lastLogicalPositions: { retained: 'cursor', [oversizedKey]: 'cursor' },
    })

    expect(decoded.lastLogicalPositions).toEqual({ retained: 'cursor' })
  })

  it('drops preference records with oversized UTF-8 values', () => {
    const oversizedValue = 'é'.repeat(MAX_PREFERENCE_RECORD_VALUE_BYTES / 2 + 1)

    const decoded = decodeBrowserPreferences({
      keyOverrides: { retained: 'Shift+K', oversized: oversizedValue },
    })

    expect(decoded.keyOverrides).toEqual({ retained: 'Shift+K' })
  })

  it('bounds aggregate bytes retained from preference records', () => {
    const lastLogicalPositions = Object.fromEntries(
      Array.from({ length: MAX_SAVED_LOGICAL_POSITIONS }, (_, index) => [
        `session-${index}`,
        'x'.repeat(MAX_PREFERENCE_RECORD_VALUE_BYTES),
      ]),
    )

    const decoded = decodeBrowserPreferences({ lastLogicalPositions })
    const retainedBytes = Object.entries(decoded.lastLogicalPositions).reduce(
      (total, [key, recordValue]) => total + new TextEncoder().encode(key + recordValue).byteLength,
      0,
    )

    expect(retainedBytes).toBeLessThanOrEqual(MAX_PREFERENCE_STORAGE_BYTES)
  })

  it('rejects an oversized stored preference body before parsing', () => {
    const getItem = vi.fn(() => ' '.repeat(MAX_PREFERENCE_STORAGE_BYTES + 1))
    vi.stubGlobal('localStorage', { getItem, setItem: vi.fn() })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
    expect(getItem).toHaveBeenCalledOnce()
  })

  it('bounds preference records before saving them', () => {
    const setItem = vi.fn()
    vi.stubGlobal('localStorage', { getItem: vi.fn(), setItem })
    const oversizedValue = 'x'.repeat(MAX_PREFERENCE_RECORD_VALUE_BYTES + 1)

    saveBrowserPreferences({
      ...defaultBrowserPreferences,
      keyOverrides: { retained: 'Shift+K', oversized: oversizedValue },
    })

    expect(JSON.parse(setItem.mock.calls[0]?.[1] as string).keyOverrides).toEqual({
      retained: 'Shift+K',
    })
  })

  it('falls back when browser storage cannot be read or written', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new DOMException('blocked')
      },
      setItem: () => {
        throw new DOMException('quota')
      },
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
    expect(() => saveBrowserPreferences(defaultBrowserPreferences)).not.toThrow()
  })

  it('falls back when resolving the browser storage getter throws', () => {
    Object.defineProperty(globalThis, 'localStorage', {
      configurable: true,
      get: () => {
        throw new DOMException('blocked', 'SecurityError')
      },
    })

    expect(loadBrowserPreferences()).toEqual(defaultBrowserPreferences)
    expect(() => saveBrowserPreferences(defaultBrowserPreferences)).not.toThrow()
  })
})
